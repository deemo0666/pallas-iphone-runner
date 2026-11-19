## 2026-11-11 build 11：已保存私钥格式契约 / Saved private-key format contract

原因：钉死的 isideload 通过 `store_data` 保存 PKCS#8 DER（字符串存储层为 Base64），安装器复用路径却用 `retrieve` + `from_pkcs8_pem`，导致再次安装或续签时解析失败。改为 `retrieve_data` + `from_pkcs8_der`，继续按公钥匹配原证书；不删除密钥、不撤销证书。

Cause: the pinned isideload writes PKCS#8 DER via `store_data` (Base64 at the string-storage layer), while reuse incorrectly parsed `retrieve` as PEM. Read with `retrieve_data` and `from_pkcs8_der`, retaining public-key matching without deleting keys or revoking certificates.

维护契约：改动签名存储或升级 isideload 时，必须核对实际写入格式并验证保存后的再次读取，不能仅测试首次申请或直接构造的内存密钥。`package-runner-kit.sh` 在构建前强制执行 `saved_key_round_trip_survives_repeat_installs_without_replacing_storage`；失败即停止打包。测试覆盖二次复用、公钥一致、存储未变，以及损坏 Base64/DER 的稳定错误与原数据保留，不读取真实用户私钥。

Maintenance contract: signing-storage changes or isideload upgrades must verify the actual writer format and read-after-save behavior, not only first issuance or directly constructed in-memory keys. Packaging runs this regression test as a mandatory gate. It covers repeated reuse, public-key equality, unchanged storage, and corrupt Base64/DER handling without accessing real user keys.
