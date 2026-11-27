pub mod cache;
pub mod classify;
pub mod robots;
pub mod rules;
pub mod source;

pub use cache::{SiteCache, SiteCacheEntry};
pub use classify::{
    classify_response, classify_response_with_context, classify_response_with_headers,
    classify_response_with_parsed, detect_challenge, detect_interstitial_challenge,
    detect_js_shell, parse_retry_after_secs, BlockReason, ResponseClassification,
};
pub use rules::{parent_domain, SiteRule, SiteRules};
pub use source::{
    compute_safe_referer, is_local_or_private_host, is_loopback_host, is_markdown_content_type,
    is_mirror_acceptable_content_type, is_safe_alternate_url, is_safe_redirect_url,
    is_safe_subresource_url, parse_markdown_link_header, resolve_raw_source_candidate,
    RawSourceCandidate, RawSourceKind,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum ResourcePolicy {
    #[default]
    None,
    Manifest,
    Transcripts,
    Diagrams,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum ToolPolicy {
    #[default]
    None,
    Manifest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum RenderMode {
    #[default]
    Auto,
    #[value(aliases = ["browser", "render", "js"])]
    Cef,
    #[value(aliases = ["kitesurf", "remote"])]
    Cdp,
    #[value(aliases = ["network", "fast", "no-js", "nojs"])]
    Cronet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum SourceMode {
    #[default]
    Auto,
    Negotiate,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimaryTransport {
    Cronet,
    Cef,
    Cdp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscalationBackend {
    None,
    Cef,
    Cdp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StrategyPlan {
    pub primary: PrimaryTransport,
    pub escalation: EscalationBackend,
    pub allow_cronet_fallback: bool,
}

pub fn plan_strategy(
    render_mode: RenderMode,
    rule: Option<&SiteRule>,
    cached: Option<&SiteCacheEntry>,
    cef_available: bool,
    cdp_available: bool,
    prefer_cdp: bool,
) -> StrategyPlan {
    let default_escalation = if prefer_cdp && cdp_available {
        EscalationBackend::Cdp
    } else if cef_available {
        EscalationBackend::Cef
    } else if cdp_available {
        EscalationBackend::Cdp
    } else {
        EscalationBackend::None
    };

    match render_mode {
        RenderMode::Cronet => StrategyPlan {
            primary: PrimaryTransport::Cronet,
            escalation: EscalationBackend::None,
            allow_cronet_fallback: false,
        },
        RenderMode::Cef => StrategyPlan {
            primary: PrimaryTransport::Cef,
            escalation: EscalationBackend::None,
            allow_cronet_fallback: false,
        },
        RenderMode::Cdp => StrategyPlan {
            primary: PrimaryTransport::Cdp,
            escalation: EscalationBackend::None,
            allow_cronet_fallback: false,
        },
        RenderMode::Auto => {
            let rule_render = rule.and_then(|r| r.render.as_deref());
            match rule_render {
                Some("cdp") => {
                    if cdp_available {
                        StrategyPlan {
                            primary: PrimaryTransport::Cdp,
                            escalation: EscalationBackend::None,
                            allow_cronet_fallback: true,
                        }
                    } else {
                        StrategyPlan {
                            primary: PrimaryTransport::Cronet,
                            escalation: default_escalation,
                            allow_cronet_fallback: false,
                        }
                    }
                }
                Some("cef") => {
                    if prefer_cdp && cdp_available {
                        StrategyPlan {
                            primary: PrimaryTransport::Cdp,
                            escalation: EscalationBackend::None,
                            allow_cronet_fallback: true,
                        }
                    } else if cef_available {
                        StrategyPlan {
                            primary: PrimaryTransport::Cef,
                            escalation: EscalationBackend::None,
                            allow_cronet_fallback: true,
                        }
                    } else {
                        StrategyPlan {
                            primary: PrimaryTransport::Cronet,
                            escalation: default_escalation,
                            allow_cronet_fallback: false,
                        }
                    }
                }
                Some("cronet") => StrategyPlan {
                    primary: PrimaryTransport::Cronet,
                    escalation: default_escalation,
                    allow_cronet_fallback: false,
                },
                _ => match cached.map(|c| c.strategy.as_str()) {
                    Some("cdp") if cdp_available => StrategyPlan {
                        primary: PrimaryTransport::Cdp,
                        escalation: EscalationBackend::None,
                        allow_cronet_fallback: true,
                    },
                    Some("cef") if prefer_cdp && cdp_available => StrategyPlan {
                        primary: PrimaryTransport::Cdp,
                        escalation: EscalationBackend::None,
                        allow_cronet_fallback: true,
                    },
                    Some("cef") if cef_available => StrategyPlan {
                        primary: PrimaryTransport::Cef,
                        escalation: EscalationBackend::None,
                        allow_cronet_fallback: true,
                    },
                    _ => StrategyPlan {
                        primary: PrimaryTransport::Cronet,
                        escalation: default_escalation,
                        allow_cronet_fallback: false,
                    },
                },
            }
        }
    }
}
