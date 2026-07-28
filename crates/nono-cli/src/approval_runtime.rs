use crate::command_policy::{
    ApprovalBackendConfig, ApprovalBackendType, ApprovalChainMode, CommandPoliciesConfig,
    REMOVED_TERMINAL_APPROVAL_MESSAGE,
};
use nono::{ApprovalBackend, ApprovalDecision, ApprovalRequest, NonoError, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

const WEBHOOK_RESPONSE_LIMIT_BYTES: u64 = 64 * 1024;

pub(crate) fn build_proxy_approval_registry(
    config: Option<&CommandPoliciesConfig>,
) -> Result<Option<nono_proxy::approval::ApprovalBackendRegistry>> {
    let Some(config) = config else {
        return Ok(None);
    };
    if config.approval_backends.is_empty() {
        return Ok(None);
    }

    let backends = build_approval_backends(config)?;
    Ok(Some(nono_proxy::approval::ApprovalBackendRegistry::new(
        config.approval_defaults.backend.clone(),
        backends,
    )))
}

pub(crate) fn build_approval_registry(
    config: &CommandPoliciesConfig,
) -> Result<nono_proxy::approval::ApprovalBackendRegistry> {
    let backends = build_approval_backends(config)?;
    Ok(nono_proxy::approval::ApprovalBackendRegistry::new(
        config.approval_defaults.backend.clone(),
        backends,
    ))
}

pub(crate) fn build_supervisor_approval_backend(
    config: Option<&CommandPoliciesConfig>,
) -> Result<Arc<dyn ApprovalBackend>> {
    let Some(config) = config else {
        return Ok(Arc::new(UnconfiguredApproval));
    };
    if config.approval_defaults.backend.is_none() {
        return Ok(Arc::new(UnconfiguredApproval));
    }

    let registry = build_approval_registry(config)?;
    let (_, backend) = registry.resolve(None)?;
    Ok(backend)
}

fn build_approval_backends(
    config: &CommandPoliciesConfig,
) -> Result<BTreeMap<String, Arc<dyn ApprovalBackend>>> {
    let mut built = BTreeMap::new();
    let mut visiting = BTreeSet::new();
    for name in config.approval_backends.keys() {
        build_approval_backend(name, config, &mut built, &mut visiting)?;
    }
    Ok(built)
}

fn build_approval_backend(
    name: &str,
    config: &CommandPoliciesConfig,
    built: &mut BTreeMap<String, Arc<dyn ApprovalBackend>>,
    visiting: &mut BTreeSet<String>,
) -> Result<Arc<dyn ApprovalBackend>> {
    if let Some(backend) = built.get(name) {
        return Ok(Arc::clone(backend));
    }
    if !visiting.insert(name.to_string()) {
        return Err(NonoError::ConfigParse(format!(
            "approval backend chain contains a cycle at '{name}'"
        )));
    }

    let backend_config = config
        .approval_backends
        .get(name)
        .ok_or_else(|| NonoError::ConfigParse(format!("unknown approval backend '{name}'")))?;
    let backend: Arc<dyn ApprovalBackend> = match backend_config.backend_type {
        ApprovalBackendType::Terminal => {
            return Err(NonoError::ConfigParse(format!(
                "approval backend '{name}' uses removed type terminal; {REMOVED_TERMINAL_APPROVAL_MESSAGE}"
            )));
        }
        ApprovalBackendType::Webhook => Arc::new(WebhookApproval::new(name, backend_config)?),
        ApprovalBackendType::Chain => {
            let mode = backend_config.mode.ok_or_else(|| {
                NonoError::ConfigParse(format!("approval backend '{name}' chain missing mode"))
            })?;
            let mut children = Vec::with_capacity(backend_config.backends.len());
            for child in &backend_config.backends {
                children.push(build_approval_backend(child, config, built, visiting)?);
            }
            Arc::new(ChainApproval {
                name: name.to_string(),
                mode,
                backends: children,
            })
        }
    };

    visiting.remove(name);
    built.insert(name.to_string(), Arc::clone(&backend));
    Ok(backend)
}

struct UnconfiguredApproval;

impl ApprovalBackend for UnconfiguredApproval {
    fn request_approval(&self, _request: &ApprovalRequest) -> Result<ApprovalDecision> {
        Ok(ApprovalDecision::Denied {
            reason: "command_policies.approval_defaults.backend is not configured".to_string(),
        })
    }

    fn backend_name(&self) -> &str {
        "unconfigured"
    }
}

struct WebhookApproval {
    name: String,
    url: String,
    timeout: Duration,
    http: ureq::Agent,
}

#[derive(Serialize)]
struct WebhookApprovalRequest<'a> {
    backend: &'a str,
    request: &'a ApprovalRequest,
}

#[derive(Deserialize)]
struct WebhookDecisionResponse {
    decision: String,
    #[serde(default)]
    reason: Option<String>,
}

impl WebhookApproval {
    fn new(name: &str, config: &ApprovalBackendConfig) -> Result<Self> {
        let url = config.url.clone().ok_or_else(|| {
            NonoError::ConfigParse(format!("approval backend '{name}' webhook missing url"))
        })?;
        let timeout = Duration::from_secs(config.timeout_secs.unwrap_or(60));
        let tls_config = ureq::tls::TlsConfig::builder()
            .root_certs(ureq::tls::RootCerts::PlatformVerifier)
            .build();
        let http = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .tls_config(tls_config)
            .build()
            .new_agent();
        Ok(Self {
            name: name.to_string(),
            url,
            timeout,
            http,
        })
    }

    fn parse_response(&self, body: &str) -> Result<ApprovalDecision> {
        if let Ok(decision) = serde_json::from_str::<ApprovalDecision>(body) {
            return Ok(decision);
        }

        let response: WebhookDecisionResponse = serde_json::from_str(body).map_err(|e| {
            NonoError::SandboxInit(format!(
                "approval webhook '{}' returned invalid JSON: {e}",
                self.name
            ))
        })?;
        match response.decision.trim().to_ascii_lowercase().as_str() {
            "grant" | "granted" | "approve" | "approved" | "allow" | "allowed" => {
                Ok(ApprovalDecision::Granted)
            }
            "deny" | "denied" | "reject" | "rejected" | "block" | "blocked" => {
                Ok(ApprovalDecision::Denied {
                    reason: response.reason.unwrap_or_else(|| {
                        format!("approval webhook '{}' denied request", self.name)
                    }),
                })
            }
            "timeout" | "timed_out" => Ok(ApprovalDecision::Denied {
                reason: format!("approval webhook '{}' timed out", self.name),
            }),
            other => Err(NonoError::SandboxInit(format!(
                "approval webhook '{}' returned unknown decision '{other}'",
                self.name
            ))),
        }
    }
}

impl ApprovalBackend for WebhookApproval {
    fn request_approval(&self, request: &ApprovalRequest) -> Result<ApprovalDecision> {
        let body = serde_json::to_vec(&WebhookApprovalRequest {
            backend: &self.name,
            request,
        })
        .map_err(|e| {
            NonoError::SandboxInit(format!(
                "failed to serialize approval webhook request '{}': {e}",
                self.name
            ))
        })?;

        let mut response = self
            .http
            .post(&self.url)
            .config()
            .http_status_as_error(false)
            .build()
            .header("Content-Type", "application/json")
            .header(
                "User-Agent",
                &format!("nono-cli/{}", env!("CARGO_PKG_VERSION")),
            )
            .send(body)
            .map_err(|e| {
                NonoError::SandboxInit(format!("approval webhook '{}' failed: {e}", self.name))
            })?;

        let status = response.status().as_u16();
        let response_body = response
            .body_mut()
            .with_config()
            .limit(WEBHOOK_RESPONSE_LIMIT_BYTES)
            .read_to_string()
            .map_err(|e| {
                NonoError::SandboxInit(format!(
                    "failed to read approval webhook '{}' response: {e}",
                    self.name
                ))
            })?;

        if !(200..300).contains(&status) {
            return Ok(ApprovalDecision::Denied {
                reason: format!(
                    "approval webhook '{}' returned HTTP {} after {:?}",
                    self.name, status, self.timeout
                ),
            });
        }

        self.parse_response(&response_body)
    }

    fn backend_name(&self) -> &str {
        &self.name
    }
}

struct ChainApproval {
    name: String,
    mode: ApprovalChainMode,
    backends: Vec<Arc<dyn ApprovalBackend>>,
}

impl ApprovalBackend for ChainApproval {
    fn request_approval(&self, request: &ApprovalRequest) -> Result<ApprovalDecision> {
        match self.mode {
            ApprovalChainMode::All => self.request_all(request),
            ApprovalChainMode::Any => Ok(self.request_any(request)),
        }
    }

    fn backend_name(&self) -> &str {
        &self.name
    }
}

impl ChainApproval {
    fn request_all(&self, request: &ApprovalRequest) -> Result<ApprovalDecision> {
        if self.backends.is_empty() {
            return Ok(ApprovalDecision::Denied {
                reason: format!("{} had no granting backend", self.name),
            });
        }
        for backend in &self.backends {
            match backend.request_approval(request)? {
                ApprovalDecision::Granted => {}
                ApprovalDecision::Denied { reason } => {
                    return Ok(ApprovalDecision::Denied {
                        reason: format!(
                            "{} denied via {}: {reason}",
                            self.name,
                            backend.backend_name()
                        ),
                    });
                }
                ApprovalDecision::Timeout => {
                    return Ok(ApprovalDecision::Denied {
                        reason: format!("{} timed out via {}", self.name, backend.backend_name()),
                    });
                }
            }
        }
        Ok(ApprovalDecision::Granted)
    }

    fn request_any(&self, request: &ApprovalRequest) -> ApprovalDecision {
        let mut reasons = Vec::new();
        for backend in &self.backends {
            match backend.request_approval(request) {
                Ok(ApprovalDecision::Granted) => return ApprovalDecision::Granted,
                Ok(ApprovalDecision::Denied { reason }) => {
                    reasons.push(format!("{} denied: {reason}", backend.backend_name()));
                }
                Ok(ApprovalDecision::Timeout) => {
                    reasons.push(format!("{} timed out", backend.backend_name()));
                }
                Err(err) => {
                    reasons.push(format!("{} errored: {err}", backend.backend_name()));
                }
            }
        }
        ApprovalDecision::Denied {
            reason: format!(
                "{} had no granting backend ({})",
                self.name,
                reasons.join("; ")
            ),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    struct StaticBackend {
        name: &'static str,
        decision: ApprovalDecision,
    }

    impl ApprovalBackend for StaticBackend {
        fn request_approval(&self, _request: &ApprovalRequest) -> Result<ApprovalDecision> {
            Ok(self.decision.clone())
        }

        fn backend_name(&self) -> &str {
            self.name
        }
    }

    fn request() -> ApprovalRequest {
        ApprovalRequest::Endpoint {
            request_id: "req-1".to_string(),
            route_id: "internal-api".to_string(),
            upstream: "https://api.internal.example".to_string(),
            method: "POST".to_string(),
            path: "/v1/tasks/1/comments".to_string(),
            rule_label: "endpoint_policy.approve[POST /v1/tasks/*/comments]".to_string(),
            reason: None,
            child_pid: 0,
            session_id: "proxy".to_string(),
        }
    }

    #[test]
    fn supervisor_without_default_backend_denies_capability_expansion() {
        let backend = build_supervisor_approval_backend(None).unwrap();

        let decision = backend.request_approval(&request()).unwrap();

        assert_eq!(backend.backend_name(), "unconfigured");
        let ApprovalDecision::Denied { reason } = decision else {
            panic!("unconfigured backend must deny requests");
        };
        assert_eq!(
            reason,
            "command_policies.approval_defaults.backend is not configured"
        );
    }

    #[test]
    fn runtime_builder_rejects_removed_terminal_backend() {
        let mut config = CommandPoliciesConfig::default();
        config.approval_defaults.backend = Some("human".to_string());
        config.approval_backends.insert(
            "human".to_string(),
            ApprovalBackendConfig {
                backend_type: ApprovalBackendType::Terminal,
                url: None,
                timeout_secs: None,
                mode: None,
                backends: Vec::new(),
            },
        );

        let Err(error) = build_supervisor_approval_backend(Some(&config)) else {
            panic!("terminal backend must be rejected");
        };

        assert_eq!(
            error.to_string(),
            format!(
                "Configuration parse error: approval backend 'human' uses removed type terminal; {REMOVED_TERMINAL_APPROVAL_MESSAGE}"
            )
        );
    }

    #[test]
    fn supervisor_uses_configured_default_backend() {
        let mut config = CommandPoliciesConfig::default();
        config.approval_defaults.backend = Some("security-review".to_string());
        config.approval_backends.insert(
            "security-review".to_string(),
            ApprovalBackendConfig {
                backend_type: ApprovalBackendType::Webhook,
                url: Some("https://approvals.example/review".to_string()),
                timeout_secs: Some(1),
                mode: None,
                backends: Vec::new(),
            },
        );

        let backend = build_supervisor_approval_backend(Some(&config)).unwrap();

        assert_eq!(backend.backend_name(), "security-review");
    }

    #[test]
    fn chain_all_requires_every_backend_to_grant() {
        let chain = ChainApproval {
            name: "all".to_string(),
            mode: ApprovalChainMode::All,
            backends: vec![
                Arc::new(StaticBackend {
                    name: "a",
                    decision: ApprovalDecision::Granted,
                }),
                Arc::new(StaticBackend {
                    name: "b",
                    decision: ApprovalDecision::Denied {
                        reason: "no".to_string(),
                    },
                }),
            ],
        };

        assert!(chain.request_approval(&request()).unwrap().is_denied());
    }

    #[test]
    fn empty_chain_all_denies_instead_of_granting() {
        let chain = ChainApproval {
            name: "all".to_string(),
            mode: ApprovalChainMode::All,
            backends: Vec::new(),
        };

        assert!(chain.request_approval(&request()).unwrap().is_denied());
    }

    #[test]
    fn chain_any_grants_if_one_backend_grants() {
        let chain = ChainApproval {
            name: "any".to_string(),
            mode: ApprovalChainMode::Any,
            backends: vec![
                Arc::new(StaticBackend {
                    name: "a",
                    decision: ApprovalDecision::Denied {
                        reason: "no".to_string(),
                    },
                }),
                Arc::new(StaticBackend {
                    name: "b",
                    decision: ApprovalDecision::Granted,
                }),
            ],
        };

        assert!(chain.request_approval(&request()).unwrap().is_granted());
    }

    #[test]
    fn webhook_response_parser_accepts_simple_decision_shape() {
        let backend = WebhookApproval {
            name: "security-review".to_string(),
            url: "https://approval.example".to_string(),
            timeout: Duration::from_secs(1),
            http: ureq::Agent::new_with_defaults(),
        };

        assert!(
            backend
                .parse_response(r#"{"decision":"granted"}"#)
                .unwrap()
                .is_granted()
        );
        assert!(
            backend
                .parse_response(r#"{"decision":"denied","reason":"policy"}"#)
                .unwrap()
                .is_denied()
        );
        let timeout = backend.parse_response(r#"{"decision":"timeout"}"#).unwrap();
        let ApprovalDecision::Denied { reason } = timeout else {
            panic!("webhook timeout must fail closed as a denial");
        };
        assert_eq!(reason, "approval webhook 'security-review' timed out");
    }
}
