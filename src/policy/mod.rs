pub mod cache;
pub mod classify;
pub mod robots;
pub mod rules;

pub use cache::{SiteCache, SiteCacheEntry};
pub use classify::{
    classify_response, classify_response_with_parsed, detect_challenge,
    detect_interstitial_challenge, detect_js_shell, ResponseClassification,
};
pub use rules::{parent_domain, SiteRule, SiteRules};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum RenderMode {
    #[default]
    Auto,
    #[value(aliases = ["browser", "render", "js"])]
    Cef,
    #[value(aliases = ["network", "fast", "no-js", "nojs"])]
    Cronet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimaryTransport {
    Cronet,
    Cef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StrategyPlan {
    pub primary: PrimaryTransport,
    pub allow_cef_escalation: bool,
    pub allow_cronet_fallback: bool,
}

pub fn plan_strategy(
    render_mode: RenderMode,
    rule: Option<&SiteRule>,
    cached: Option<&SiteCacheEntry>,
    cef_available: bool,
) -> StrategyPlan {
    match render_mode {
        RenderMode::Cronet => StrategyPlan {
            primary: PrimaryTransport::Cronet,
            allow_cef_escalation: false,
            allow_cronet_fallback: false,
        },
        RenderMode::Cef => StrategyPlan {
            primary: PrimaryTransport::Cef,
            allow_cef_escalation: false,
            allow_cronet_fallback: false,
        },
        RenderMode::Auto => {
            if !cef_available {
                return StrategyPlan {
                    primary: PrimaryTransport::Cronet,
                    allow_cef_escalation: false,
                    allow_cronet_fallback: false,
                };
            }

            let rule_render = rule.and_then(|r| r.render.as_deref());
            match rule_render {
                Some("cef") => StrategyPlan {
                    primary: PrimaryTransport::Cef,
                    allow_cef_escalation: false,
                    allow_cronet_fallback: true,
                },
                Some("cronet") => StrategyPlan {
                    primary: PrimaryTransport::Cronet,
                    allow_cef_escalation: true,
                    allow_cronet_fallback: false,
                },
                _ => {
                    if cached.map(|c| c.strategy.as_str()) == Some("cef") {
                        StrategyPlan {
                            primary: PrimaryTransport::Cef,
                            allow_cef_escalation: false,
                            allow_cronet_fallback: true,
                        }
                    } else {
                        StrategyPlan {
                            primary: PrimaryTransport::Cronet,
                            allow_cef_escalation: true,
                            allow_cronet_fallback: false,
                        }
                    }
                }
            }
        }
    }
}
