use crate::{
    account::{self, AccountInfo},
    protocol::{self, Failure, Result, Safe},
    state,
};
use apple_codesign_quick::{
    BundleSigningSettings, ProvisioningProfile, RustCryptoCmsSigner, sign_bundle,
};
use core_foundation::{
    base::{CFType, CFTypeRef, TCFType},
    boolean::CFBoolean,
    data::CFData,
    dictionary::{CFDictionary, CFDictionaryRef},
    string::CFString,
    url::CFURL,
};
use isideload::{
    SideloadError,
    dev::{
        certificates::{CertificatesApi, DevelopmentCertificate},
        developer_session::{DeveloperSession, DeveloperTeam},
    },
    sideload::{builder::MaxCertsBehavior, cert_identity::CertificateIdentity},
    util::{callbacks::MaxCertsCallbackBox, storage::SideloadingStorage},
};
use plist::{Dictionary, Value};
use rsa::{RsaPrivateKey, pkcs1::EncodeRsaPublicKey, pkcs8::DecodePrivateKey};
use security_framework::{
    identity::SecIdentity,
    item::{ItemClass, ItemSearchOptions, Limit, Reference, SearchResult},
    os::macos::code_signing::{Flags, SecStaticCode},
};
use security_framework_sys::code_signing::SecStaticCodeRef;
use std::{fs, path::Path, ptr};
use x509_cert::{
    Certificate,
    der::{Decode, Encode},
};
use zeroize::Zeroizing;

// Only one credential is owned for the whole signing operation; it is never copied.
#[allow(clippy::large_enum_variant)]
pub enum Credential {
    Keychain {
        identity: SecIdentity,
        certificate: Certificate,
    },
    Managed(CertificateIdentity),
}

impl Credential {
    pub async fn retrieve(
        info: &AccountInfo,
        session: &mut DeveloperSession,
        team: &DeveloperTeam,
    ) -> Result<Self> {
        protocol::activity("正在检查原有签名证书…");
        // A failed lookup must never turn into a request for another certificate.
        let remote = session
            .list_ios_certs(team)
            .await
            .map_err(certificate_error)?;
        let now = chrono::Utc::now().timestamp();
        let mut eligible: Vec<_> = remote
            .iter()
            .filter_map(|record| {
                let certificate =
                    Certificate::from_der(record.cert_content.as_ref()?.as_ref()).ok()?;
                valid_at(&certificate, now).then_some((record, certificate))
            })
            .collect();
        eligible.sort_by_key(|(_, cert)| std::cmp::Reverse(expiry(cert)));

        let identities = match ItemSearchOptions::new()
            .class(ItemClass::identity())
            .load_refs(true)
            .limit(Limit::All)
            .search()
        {
            Ok(items) => items,
            Err(error) if error.code() == -25300 => vec![], // errSecItemNotFound
            Err(error) => return Err(keychain_error(error.code())),
        };
        let identities: Vec<_> = identities
            .into_iter()
            .filter_map(|item| match item {
                SearchResult::Ref(Reference::Identity(identity)) => Some(identity),
                _ => None,
            })
            .map(|identity| {
                let der = identity
                    .certificate()
                    .map_err(|e| keychain_error(e.code()))?
                    .to_der();
                Ok((identity, der))
            })
            .collect::<Result<_>>()?;
        for (record, certificate) in &eligible {
            if let Some((identity, _)) = identities.iter().find(|(_, der)| {
                record
                    .cert_content
                    .as_ref()
                    .is_some_and(|data| data.as_ref() == der)
            }) {
                protocol::activity(
                    "已找到本机原证书，将复用于两个 App。钥匙串若询问，请允许安装器使用。",
                );
                return Ok(Self::Keychain {
                    identity: identity.clone(),
                    certificate: certificate.clone(),
                });
            }
        }

        let storage = account::storage(&info.id);
        // Match the saved key, not a product/machine label that can change between versions.
        if let Some(key) = saved_private_key(&storage, &info.email)?
            && let Some(identity) = managed_match(key, &eligible)?
        {
            protocol::activity("正在复用安装器保存的签名证书…");
            return Ok(Self::Managed(identity));
        }
        protocol::activity("本机没有可复用的有效证书，正在向 Apple 申请签名证书…");
        // Our lookup above already tried every certificate matching the saved key.
        // A fresh label keeps upstream from returning an expired certificate with an old label.
        let label = format!("Pallas Agent Installer {}", uuid::Uuid::new_v4());
        let certificate = CertificateIdentity::retrieve(
            &label,
            &info.email,
            session,
            team,
            &storage,
            &MaxCertsBehavior::<MaxCertsCallbackBox>::Error,
        )
        .await
        .map_err(certificate_error)?;
        if !valid_at(&certificate.certificate, now) {
            return Err(Failure::new(
                "certificate_expired",
                "Apple 返回的签名证书不在有效期内，请检查 Mac 日期后重试。",
            ));
        }
        Ok(Self::Managed(certificate))
    }

    fn certificate(&self) -> &Certificate {
        match self {
            Self::Keychain { certificate, .. } => certificate,
            Self::Managed(identity) => &identity.certificate,
        }
    }

    pub fn expires(&self) -> i64 {
        expiry(self.certificate())
    }

    pub fn report_selected(&self) -> Result<()> {
        let der = self
            .certificate()
            .to_der()
            .safe("certificate", "无法读取签名证书。")?;
        protocol::emit(serde_json::json!({"type":"certificate",
            "source":match self { Self::Keychain { .. } => "keychain", Self::Managed(_) => "installer" },
            "sha256":state::hash(&der)}));
        Ok(())
    }

    pub fn validate_profile(&self, profile: &ProvisioningProfile) -> Result<()> {
        let der = self
            .certificate()
            .to_der()
            .safe("certificate", "无法读取签名证书。")?;
        validate_profile_certificate(profile.plist(), &der)
    }

    pub fn sign(
        &self,
        app: &Path,
        team: &str,
        profile: &ProvisioningProfile,
        encoded: &[u8],
    ) -> Result<()> {
        match self {
            Self::Managed(identity) => {
                let signer = RustCryptoCmsSigner::new(
                    identity.private_key.clone(),
                    identity.certificate.clone(),
                    identity
                        .profile_to_certificate_chain(profile)
                        .safe("certificate", "无法验证签名证书链。")?,
                );
                let mut settings =
                    BundleSigningSettings::new(team, profile.entitlements().clone(), Some(&signer));
                settings.embedded_mobileprovision = Some(encoded);
                sign_bundle(app, &settings).safe("signing", "App 签名失败，尚未安装任何 App。")
            }
            Self::Keychain { identity, .. } => {
                fs::write(app.join("embedded.mobileprovision"), encoded)
                    .safe("profile", "无法写入 App 的描述文件。")?;
                sign_native_bundle(app, team, identity, profile.entitlements())
            }
        }
    }
}

fn managed_match(
    key: RsaPrivateKey,
    eligible: &[(&DevelopmentCertificate, Certificate)],
) -> Result<Option<CertificateIdentity>> {
    let public = key
        .to_public_key()
        .to_pkcs1_der()
        .safe("certificate_keychain", "无法读取签名密钥的公钥。")?;
    let found = eligible.iter().find(|(_, cert)| {
        cert.tbs_certificate
            .subject_public_key_info
            .subject_public_key
            .as_bytes()
            == Some(public.as_bytes())
    });
    Ok(found.map(|(record, certificate)| CertificateIdentity {
        machine_id: record.machine_id.clone().unwrap_or_default(),
        machine_name: record.machine_name.clone().unwrap_or_default(),
        private_key: key,
        certificate: certificate.clone(),
    }))
}

fn expiry(cert: &Certificate) -> i64 {
    cert.tbs_certificate
        .validity
        .not_after
        .to_unix_duration()
        .as_secs() as i64
}
fn valid_at(cert: &Certificate, now: i64) -> bool {
    cert.tbs_certificate
        .validity
        .not_before
        .to_unix_duration()
        .as_secs() as i64
        <= now
        && now < expiry(cert)
}

fn validate_profile_certificate(profile: &Dictionary, der: &[u8]) -> Result<()> {
    if !profile
        .get("DeveloperCertificates")
        .and_then(Value::as_array)
        .is_some_and(|certs| certs.iter().any(|cert| cert.as_data() == Some(der)))
    {
        return Err(Failure::new(
            "profile_certificate",
            "描述文件不包含本次签名证书，已停止安装。请稍后重试。",
        ));
    }
    Ok(())
}

fn keychain_error(code: i32) -> Failure {
    Failure::new(
        "certificate_keychain",
        "无法使用本机原证书。请打开安装器，解锁登录钥匙串并允许使用签名密钥后重试。",
    )
    .detail("Keychain", code.into())
}

fn certificate_error(report: rootcause::Report) -> Failure {
    for node in report.iter_reports() {
        if let Some(SideloadError::DeveloperError(code, _)) =
            node.downcast_current_context::<SideloadError>()
        {
            return if *code == 7460 {
                Failure::new("certificate_limit", "Apple 的签名证书名额已满，本机没有找到可用私钥。请在原来成功安装的 Mac 上重试；原证书未被撤销。")
            } else {
                Failure::new("certificate_apple", "Apple 拒绝了签名证书请求，请保留错误码后重试。")
            }.detail("Apple", *code);
        }
        if let Some(error) = node.downcast_current_context::<security_framework::base::Error>() {
            return keychain_error(error.code());
        }
        if node.downcast_current_context::<keyring::Error>().is_some() {
            return Failure::new(
                "certificate_keychain",
                "无法读取签名密钥，请解锁此 Mac 的登录钥匙串后重试。",
            );
        }
        if let Some(error) = node.downcast_current_context::<reqwest::Error>() {
            let failure = Failure::new(
                "certificate_network",
                "无法连接 Apple 证书服务，请检查网络后重试。原证书未被更改。",
            );
            return match error.status() {
                Some(status) => failure.detail("HTTP", status.as_u16().into()),
                None => failure,
            };
        }
    }
    Failure::new(
        "certificate",
        "未能获取签名证书，已保留原证书。请检查网络后重试，或联系维护者查看错误码。",
    )
}

// Security.framework's signer uses SecIdentity directly: the private key stays in
// Keychain and the renewal process's no-UI guard also covers signing.
// API declarations: apple-oss-distributions/Security, SecCodeSigner.h (macOS 10.5+).
#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    fn SecCodeSignerCreate(parameters: CFDictionaryRef, flags: u32, signer: *mut CFTypeRef) -> i32;
    fn SecCodeSignerAddSignature(signer: CFTypeRef, code: SecStaticCodeRef, flags: u32) -> i32;
}

fn sign_native_bundle(
    app: &Path,
    team: &str,
    identity: &SecIdentity,
    entitlements: &Dictionary,
) -> Result<()> {
    let mut nested = Vec::new();
    for entry in walkdir::WalkDir::new(app).min_depth(1) {
        let entry = entry.safe("signing", "无法检查 App 内的签名组件。")?;
        if !entry.file_type().is_symlink()
            && entry.path().extension().is_some_and(|e| {
                ["framework", "xctest", "dylib", "app"]
                    .iter()
                    .any(|extension| e == *extension)
            })
        {
            nested.push(entry.into_path());
        }
    }
    nested.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for path in nested {
        native_sign(&path, team, identity, None)?;
    }
    native_sign(app, team, identity, Some(entitlements))
}

fn native_sign(
    path: &Path,
    team: &str,
    identity: &SecIdentity,
    entitlements: Option<&Dictionary>,
) -> Result<()> {
    let url =
        CFURL::from_path(path, path.is_dir()).ok_or(Failure::new("signing", "签名路径无效。"))?;
    let code = SecStaticCode::from_path(&url, Flags::NONE).map_err(|e| signing_error(e.code()))?;
    let mut parameters = vec![
        (CFString::new("signer"), identity.as_CFType()),
        (
            CFString::new("teamidentifier"),
            CFString::new(team).as_CFType(),
        ),
        (
            CFString::new("timestamp-required"),
            CFBoolean::false_value().as_CFType(),
        ),
    ];
    if let Some(entitlements) = entitlements {
        let mut xml = Vec::new();
        Value::Dictionary(entitlements.clone())
            .to_writer_xml(&mut xml)
            .safe("signing", "无法写入签名权限。")?;
        // SecCodeSigner takes a complete EmbeddedEntitlements blob, not raw XML.
        let size = u32::try_from(xml.len() + 8)
            .map_err(|_| Failure::new("signing", "无法写入签名权限。"))?;
        let mut blob = Vec::with_capacity(size as usize);
        blob.extend_from_slice(&0xfade7171u32.to_be_bytes());
        blob.extend_from_slice(&size.to_be_bytes());
        blob.extend_from_slice(&xml);
        parameters.push((
            CFString::new("entitlements"),
            CFData::from_buffer(&blob).as_CFType(),
        ));
    }
    let parameters = CFDictionary::from_CFType_pairs(&parameters);
    let mut signer = ptr::null();
    // Generate DER entitlements for current iOS.
    let status =
        unsafe { SecCodeSignerCreate(parameters.as_concrete_TypeRef(), 1 << 9, &mut signer) };
    if status != 0 {
        return Err(signing_error(status));
    }
    if signer.is_null() {
        return Err(Failure::new("signing", "无法创建本机签名器。"));
    }
    let signer = unsafe { CFType::wrap_under_create_rule(signer) };
    let status =
        unsafe { SecCodeSignerAddSignature(signer.as_CFTypeRef(), code.as_concrete_TypeRef(), 0) };
    if status == 0 {
        Ok(())
    } else {
        Err(signing_error(status))
    }
}

fn signing_error(code: i32) -> Failure {
    match code {
        -25293 | -25308 | -128 => keychain_error(code),
        _ => Failure::new(
            "signing",
            "使用原证书签名失败，尚未安装任何 App。请保留错误码。",
        )
        .detail("Security", code.into()),
    }
}

// isideload stores PKCS#8 DER through store_data (Base64 at the string-storage layer).
// Read through the same binary API; parsing retrieve() as PEM breaks repeat installs.
fn saved_private_key(
    storage: &dyn SideloadingStorage,
    email: &str,
) -> Result<Option<RsaPrivateKey>> {
    let message = "安装器保存的签名密钥无法读取，已保留原证书，请联系维护者。";
    let stored = storage
        .retrieve_data(&format!("{}/key", state::hash(email.as_bytes())))
        .safe("certificate_keychain", message)?;
    stored
        .map(|bytes| {
            let bytes = Zeroizing::new(bytes);
            RsaPrivateKey::from_pkcs8_der(&bytes).safe("certificate_keychain", message)
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_key_round_trip_survives_repeat_installs_without_replacing_storage() {
        use isideload::util::storage::InMemoryStorage;
        use rsa::{pkcs8::EncodePrivateKey, rand_core::OsRng, traits::PublicKeyParts};
        let storage = InMemoryStorage::new();
        let email = "installer-test@example.invalid";
        let slot = format!("{}/key", state::hash(email.as_bytes()));
        assert!(saved_private_key(&storage, email).unwrap().is_none());
        let key = RsaPrivateKey::new(&mut OsRng, 2048).unwrap();
        // Exercise the exact writer used by the pinned isideload dependency.
        storage
            .store_data(&slot, key.to_pkcs8_der().unwrap().as_bytes())
            .unwrap();
        let original = Zeroizing::new(storage.retrieve(&slot).unwrap().unwrap());
        for _ in 0..2 {
            let restored = saved_private_key(&storage, email).unwrap().unwrap();
            assert_eq!(restored.n(), key.n());
            assert_eq!(restored.e(), key.e());
            assert!(storage.retrieve(&slot).unwrap().unwrap() == *original);
        }
        for corrupt in ["not-base64!", "bm90LWEtcHJpdmF0ZS1rZXk="] {
            storage.store(&slot, corrupt).unwrap();
            let failure = saved_private_key(&storage, email).unwrap_err();
            assert_eq!(failure.code, "certificate_keychain");
            assert_eq!(storage.retrieve(&slot).unwrap().unwrap(), corrupt);
            assert!(!format!("{failure:?}").contains(corrupt));
        }
    }

    #[test]
    fn profiles_must_include_the_exact_signing_certificate() {
        let mut profile = Dictionary::new();
        assert!(validate_profile_certificate(&profile, b"selected-cert").is_err());
        profile.insert(
            "DeveloperCertificates".into(),
            Value::Array(vec![Value::Data(b"other-cert".to_vec())]),
        );
        assert!(validate_profile_certificate(&profile, b"selected-cert").is_err());
        profile.insert(
            "DeveloperCertificates".into(),
            Value::Array(vec![Value::Data(b"selected-cert".to_vec())]),
        );
        validate_profile_certificate(&profile, b"selected-cert").unwrap();
    }

    #[test]
    fn certificate_errors_preserve_codes_but_never_upstream_secrets() {
        let secret = "sentinel-password-do-not-echo";
        let failure = certificate_error(
            rootcause::report!(SideloadError::DeveloperError(7460, secret.into())).into(),
        );
        assert_eq!(failure.code, "certificate_limit");
        assert_eq!(failure.detail, Some(("Apple", 7460)));
        assert!(!format!("{failure:?}").contains(secret));
        let failure = certificate_error(rootcause::report!(secret).into());
        assert_eq!(failure.code, "certificate");
        assert!(!format!("{failure:?}").contains(secret));
    }

    #[test]
    #[ignore = "creates an isolated temporary Keychain; run explicitly on macOS"]
    fn native_keychain_signing_and_saved_key_reuse() {
        use security_framework::{
            import_export::Pkcs12ImportOptions,
            os::macos::keychain::{CreateOptions, SecKeychain},
        };
        use security_framework_sys::base::SecKeychainRef;
        use std::process::Command;
        unsafe extern "C" {
            fn SecKeychainDelete(keychain: SecKeychainRef) -> i32;
        }
        struct TestKeychain(SecKeychain);
        impl Drop for TestKeychain {
            fn drop(&mut self) {
                assert_eq!(
                    unsafe { SecKeychainDelete(self.0.as_concrete_TypeRef()) },
                    0
                );
            }
        }
        fn run(dir: &Path, program: &str, args: &[&str]) {
            let output = Command::new(program)
                .current_dir(dir)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "test fixture command failed: {program}"
            );
        }
        let work = tempfile::tempdir().unwrap();
        let path = work.path();
        fs::write(path.join("cert.conf"), "[req]\nprompt=no\ndistinguished_name=dn\nx509_extensions=ext\n[dn]\nCN=Installer Test Only\nOU=ABCDE12345\n[ext]\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=codeSigning\nbasicConstraints=critical,CA:FALSE\n").unwrap();
        run(
            path,
            "/usr/bin/openssl",
            &[
                "genpkey",
                "-algorithm",
                "RSA",
                "-pkeyopt",
                "rsa_keygen_bits:2048",
                "-out",
                "key.pem",
            ],
        );
        run(
            path,
            "/usr/bin/openssl",
            &[
                "req",
                "-new",
                "-x509",
                "-key",
                "key.pem",
                "-out",
                "cert.pem",
                "-days",
                "2",
                "-config",
                "cert.conf",
            ],
        );
        run(
            path,
            "/usr/bin/openssl",
            &[
                "x509", "-in", "cert.pem", "-outform", "DER", "-out", "cert.der",
            ],
        );
        run(
            path,
            "/usr/bin/openssl",
            &[
                "pkcs12",
                "-export",
                "-inkey",
                "key.pem",
                "-in",
                "cert.pem",
                "-out",
                "identity.p12",
                "-passout",
                "pass:test-only",
            ],
        );
        let key = RsaPrivateKey::from_pkcs8_pem(&fs::read_to_string(path.join("key.pem")).unwrap())
            .unwrap();
        let der = fs::read(path.join("cert.der")).unwrap();
        let cert = Certificate::from_der(&der).unwrap();
        let begin = cert
            .tbs_certificate
            .validity
            .not_before
            .to_unix_duration()
            .as_secs() as i64;
        assert!(valid_at(&cert, begin));
        assert!(!valid_at(&cert, begin - 1));
        assert!(!valid_at(&cert, expiry(&cert)));
        let record = DevelopmentCertificate {
            name: None,
            certificate_id: None,
            serial_number: None,
            machine_id: None,
            machine_name: Some("Old Installer Name".into()),
            cert_content: Some(der.clone().into()),
            certificate_platform: None,
            certificate_type: None,
            status: None,
            status_code: None,
            expiration_date: None,
        };
        assert!(managed_match(key, &[(&record, cert)]).unwrap().is_some());

        let _no_ui = SecKeychain::disable_user_interaction().unwrap();
        let keychain = TestKeychain(
            CreateOptions::new()
                .password("test-only")
                .create(path.join("test.keychain"))
                .unwrap(),
        );
        let identities = Pkcs12ImportOptions::new()
            .passphrase("test-only")
            .keychain(keychain.0.clone())
            .import(&fs::read(path.join("identity.p12")).unwrap())
            .unwrap();
        let identity = identities[0].identity.as_ref().unwrap();
        let found = ItemSearchOptions::new()
            .class(ItemClass::identity())
            .keychains(std::slice::from_ref(&keychain.0))
            .load_refs(true)
            .limit(Limit::All)
            .search()
            .unwrap();
        assert!(found.into_iter().any(|item| matches!(item,
            SearchResult::Ref(Reference::Identity(ref candidate)) if candidate.certificate().unwrap().to_der() == der)));
        let app = path.join("Fixture.app");
        let framework = app.join("Frameworks/Fixture.framework");
        fs::write(path.join("fixture.c"), "int main(void) { return 0; }\n").unwrap();
        run(
            path,
            "/usr/bin/xcrun",
            &["clang", "fixture.c", "-o", "fixture"],
        );
        fs::create_dir_all(&framework).unwrap();
        for (bundle, name, kind) in [(&app, "Fixture", "APPL"), (&framework, "Fixture", "FMWK")] {
            fs::copy(path.join("fixture"), bundle.join(name)).unwrap();
            let info = Dictionary::from_iter([
                (
                    "CFBundleIdentifier",
                    Value::String(format!("io.example.{kind}")),
                ),
                ("CFBundleExecutable", name.into()),
                ("CFBundlePackageType", kind.into()),
                ("CFBundleVersion", "1".into()),
            ]);
            Value::Dictionary(info)
                .to_file_xml(bundle.join("Info.plist"))
                .unwrap();
        }
        let entitlements = Dictionary::from_iter([("get-task-allow", Value::Boolean(true))]);
        // Both passes use the same non-exported identity with background UI disabled.
        for _ in 0..2 {
            sign_native_bundle(&app, "ABCDE12345", identity, &entitlements).unwrap();
            crate::signing::verify_signature(&app).unwrap();
        }
        let prefix = path.join("signed-cert-");
        run(
            path,
            "/usr/bin/codesign",
            &[
                "-d",
                &format!("--extract-certificates={}", prefix.display()),
                app.to_str().unwrap(),
            ],
        );
        assert_eq!(fs::read(path.join("signed-cert-0")).unwrap(), der);
        let output = Command::new("/usr/bin/codesign")
            .args(["-d", "--entitlements", ":-"])
            .arg(app)
            .output()
            .unwrap();
        assert!(output.status.success());
        let signed_entitlements =
            Value::from_reader_xml(std::io::Cursor::new(output.stdout)).unwrap();
        assert_eq!(
            signed_entitlements
                .as_dictionary()
                .unwrap()
                .get("get-task-allow")
                .and_then(Value::as_boolean),
            Some(true)
        );
        if let Some(kit) = std::env::var_os("DSH_TEST_IPA_KIT") {
            let package = crate::signing::Package::open(Path::new(&kit)).unwrap();
            let app_identity = crate::signing::Identity::new("ABCDE12345", false).unwrap();
            let apps = package
                .extract(
                    &path.join("ipa-fixtures"),
                    &app_identity,
                    crate::signing::InstallTarget::All,
                )
                .unwrap();
            for (_, app) in apps {
                for _ in 0..2 {
                    sign_native_bundle(&app, "ABCDE12345", identity, &entitlements).unwrap();
                    crate::signing::verify_signature(&app).unwrap();
                }
            }
        }
    }
}
