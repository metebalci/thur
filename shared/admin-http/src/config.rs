// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Config shapes consumed by [`crate::run_http_server`].

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Listener config — one bind address, optional TLS termination.
#[derive(Debug, Clone)]
pub struct HttpListenerConfig {
    pub listen: String,
    pub tls: Option<TlsConfig>,
}

/// TLS material + optional mTLS trust anchor.
///
/// When both `cert_file` and `key_file` paths exist on disk, they are
/// loaded as-is. When both are absent, the daemon auto-generates a
/// self-signed cert+key pair at boot. A partial state (only one
/// present) is a hard error — see
/// [`crate::selfsign::load_or_generate_cert_key`].
///
/// `extra_sans` extends the auto-generated cert's SAN list beyond the
/// built-in hostname/loopback set. It is only consulted on the
/// generate path; a loaded operator-supplied cert ignores it.
#[derive(Debug, Clone)]
pub struct TlsConfig {
    pub cert_file: PathBuf,
    pub key_file: PathBuf,
    pub client_ca_file: Option<PathBuf>,
    pub extra_sans: Vec<String>,
}

impl TlsConfig {
    /// Coerce the YAML `http.tls` block into `Option<TlsConfig>` with
    /// the partial-config check.
    ///
    /// - cert+key both empty → `Ok(None)` (plaintext listener);
    ///   `extra_sans` is ignored (it is harmless without TLS).
    /// - cert+key both set → `Ok(Some(...))`; `client_ca_file` carried
    ///   through if non-empty; `extra_sans` trimmed, empties dropped.
    /// - only one of cert/key set → `Err(...)` (partial config).
    /// - only `client_ca_file` set → `Err(...)` (mTLS without server cert).
    pub fn from_yaml(
        cert_file: &str,
        key_file: &str,
        client_ca_file: &str,
        extra_sans: &[String],
    ) -> Result<Option<Self>> {
        let cert = cert_file.trim();
        let key = key_file.trim();
        let ca = client_ca_file.trim();

        match (cert.is_empty(), key.is_empty()) {
            (true, true) => {
                if !ca.is_empty() {
                    bail!(
                        "http.tls.client_ca_file is set ({ca:?}) but http.tls.cert_file and \
                         http.tls.key_file are empty; mTLS requires a server cert/key"
                    );
                }
                Ok(None)
            }
            (false, false) => Ok(Some(Self {
                cert_file: PathBuf::from(cert),
                key_file: PathBuf::from(key),
                client_ca_file: (!ca.is_empty()).then(|| PathBuf::from(ca)),
                extra_sans: extra_sans
                    .iter()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
            })),
            (true, false) => bail!(
                "http.tls.cert_file is empty but http.tls.key_file is set ({key:?}); \
                 set both to enable TLS or both to empty for plaintext"
            ),
            (false, true) => bail!(
                "http.tls.key_file is empty but http.tls.cert_file is set ({cert:?}); \
                 set both to enable TLS or both to empty for plaintext"
            ),
        }
    }

    /// Load the `http.tls` block straight from a daemon conffile.
    ///
    /// Used by daemon-down CLI flows (`system regenerate-cert`) that
    /// need the cert/key paths without standing up the full daemon
    /// config. Only `http.tls` is deserialized; every other key is
    /// ignored. Routes through [`Self::from_yaml`], so the same
    /// partial-config check applies.
    pub fn load_from_conffile(path: &Path) -> Result<Option<Self>> {
        #[derive(serde::Deserialize, Default)]
        struct TlsYaml {
            #[serde(default)]
            cert_file: String,
            #[serde(default)]
            key_file: String,
            #[serde(default)]
            client_ca_file: String,
            #[serde(default)]
            extra_sans: Vec<String>,
        }
        #[derive(serde::Deserialize, Default)]
        struct HttpYaml {
            #[serde(default)]
            tls: TlsYaml,
        }
        #[derive(serde::Deserialize, Default)]
        struct ConfRoot {
            #[serde(default)]
            http: HttpYaml,
        }

        let body = std::fs::read_to_string(path)
            .with_context(|| format!("reading conffile {}", path.display()))?;
        let root: ConfRoot = serde_yaml::from_str(&body)
            .with_context(|| format!("parsing conffile {}", path.display()))?;
        Self::from_yaml(
            &root.http.tls.cert_file,
            &root.http.tls.key_file,
            &root.http.tls.client_ca_file,
            &root.http.tls.extra_sans,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_empty_yields_none() {
        let got = TlsConfig::from_yaml("", "", "", &[]).expect("ok");
        assert!(got.is_none());
    }

    #[test]
    fn both_set_yields_some_without_ca() {
        let got = TlsConfig::from_yaml("/a/cert.pem", "/a/key.pem", "", &[])
            .expect("ok")
            .expect("some");
        assert_eq!(got.cert_file, PathBuf::from("/a/cert.pem"));
        assert_eq!(got.key_file, PathBuf::from("/a/key.pem"));
        assert!(got.client_ca_file.is_none());
        assert!(got.extra_sans.is_empty());
    }

    #[test]
    fn ca_carried_through() {
        let got = TlsConfig::from_yaml("/a/cert.pem", "/a/key.pem", "/a/ca.pem", &[])
            .expect("ok")
            .expect("some");
        assert_eq!(got.client_ca_file, Some(PathBuf::from("/a/ca.pem")));
    }

    #[test]
    fn partial_cert_only_errors() {
        let err = TlsConfig::from_yaml("/a/cert.pem", "", "", &[]).unwrap_err();
        assert!(err.to_string().contains("key_file is empty"));
    }

    #[test]
    fn partial_key_only_errors() {
        let err = TlsConfig::from_yaml("", "/a/key.pem", "", &[]).unwrap_err();
        assert!(err.to_string().contains("cert_file is empty"));
    }

    #[test]
    fn ca_without_pair_errors() {
        let err = TlsConfig::from_yaml("", "", "/a/ca.pem", &[]).unwrap_err();
        assert!(err.to_string().contains("client_ca_file is set"));
    }

    #[test]
    fn extra_sans_carried_through_trimmed() {
        let got = TlsConfig::from_yaml(
            "/a/cert.pem",
            "/a/key.pem",
            "",
            &[
                "vtl.internal".to_string(),
                "  10.0.0.5  ".to_string(),
                String::new(),
            ],
        )
        .expect("ok")
        .expect("some");
        assert_eq!(got.extra_sans, vec!["vtl.internal", "10.0.0.5"]);
    }

    #[test]
    fn extra_sans_ignored_without_pair() {
        let got = TlsConfig::from_yaml("", "", "", &["x.example".to_string()]).expect("ok");
        assert!(got.is_none());
    }

    #[test]
    fn load_from_conffile_reads_tls_block() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("thurvtl.yaml");
        std::fs::write(
            &path,
            "data_dir: /var/lib/thurvtl\n\
             http:\n  \
               listen: \"0.0.0.0:9090\"\n  \
               tls:\n    \
                 cert_file: /etc/thurvtl/tls/cert.pem\n    \
                 key_file: /etc/thurvtl/tls/key.pem\n    \
                 extra_sans:\n      \
                   - vtl.internal\n",
        )
        .expect("write conffile");

        let got = TlsConfig::load_from_conffile(&path)
            .expect("ok")
            .expect("some");
        assert_eq!(got.cert_file, PathBuf::from("/etc/thurvtl/tls/cert.pem"));
        assert_eq!(got.extra_sans, vec!["vtl.internal"]);
    }

    #[test]
    fn load_from_conffile_no_http_block_yields_none() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("thurvsa.yaml");
        std::fs::write(&path, "data_dir: /var/lib/thurvsa\n").expect("write conffile");

        let got = TlsConfig::load_from_conffile(&path).expect("ok");
        assert!(got.is_none());
    }
}
