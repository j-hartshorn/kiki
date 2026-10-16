#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PricingIdentity {
    pub model: &'static str,
    pub service_tier: &'static str,
    pub version: &'static str,
    pub source: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rates {
    pub uncached_input_nanos: u64,
    pub cached_input_nanos: u64,
    pub cache_write_nanos: u64,
    pub cache_write_1h_nanos: u64,
    pub output_nanos: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelPricing {
    pub identity: PricingIdentity,
    pub long_context_threshold: u64,
    pub long_context_threshold_display: &'static str,
    pub short_context: Rates,
    pub long_context: Rates,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelConfiguration {
    pub id: &'static str,
    pub thinking_levels: &'static [&'static str],
    pub default_thinking: &'static str,
    pub service_tier: &'static str,
    pub context_window_tokens: u64,
    pub context_window_display: &'static str,
    pub max_output_tokens: u64,
    pub max_output_display: &'static str,
    pub pricing: ModelPricing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    OpenAi,
    Anthropic,
    Google,
}

impl Provider {
    pub fn display_name(self) -> &'static str {
        match self {
            Self::OpenAi => "OpenAI",
            Self::Anthropic => "Anthropic",
            Self::Google => "Google",
        }
    }

    pub fn api_key_variable(self) -> &'static str {
        match self {
            Self::OpenAi => "OPENAI_API_KEY",
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::Google => "GEMINI_API_KEY",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelSelection {
    model: &'static ModelConfiguration,
    thinking: &'static str,
}

impl ModelSelection {
    pub fn new(model: &'static ModelConfiguration, thinking: &'static str) -> Option<Self> {
        model
            .supports_thinking(thinking)
            .then_some(Self { model, thinking })
    }

    pub fn model(self) -> &'static ModelConfiguration {
        self.model
    }

    pub fn thinking(self) -> &'static str {
        self.thinking
    }
}

pub static THINKING_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
pub static GOOGLE_THINKING_LEVELS: &[&str] = &["low", "medium", "high"];

pub static ASTRA: ModelConfiguration = ModelConfiguration {
    id: "openai/gpt-6-astra",
    thinking_levels: THINKING_LEVELS,
    default_thinking: "low",
    service_tier: "default",
    context_window_tokens: 1_050_000,
    context_window_display: "1.05m",
    max_output_tokens: 128_000,
    max_output_display: "128k",
    pricing: ModelPricing {
        identity: PricingIdentity {
            model: "openai/gpt-6-astra",
            service_tier: "default",
            version: "2026-09-04",
            source: "https://openai.com/api/pricing/",
        },
        long_context_threshold: 272_000,
        long_context_threshold_display: "272k",
        short_context: Rates {
            uncached_input_nanos: 10_000,
            cached_input_nanos: 1_000,
            cache_write_nanos: 12_500,
            cache_write_1h_nanos: 12_500,
            output_nanos: 50_000,
        },
        long_context: Rates {
            uncached_input_nanos: 20_000,
            cached_input_nanos: 2_000,
            cache_write_nanos: 25_000,
            cache_write_1h_nanos: 25_000,
            output_nanos: 75_000,
        },
    },
};

pub static GPT6_1_SOL: ModelConfiguration = ModelConfiguration {
    id: "openai/gpt-6.1-sol",
    thinking_levels: THINKING_LEVELS,
    default_thinking: "medium",
    service_tier: "default",
    context_window_tokens: 1_050_000,
    context_window_display: "1.05m",
    max_output_tokens: 128_000,
    max_output_display: "128k",
    pricing: ModelPricing {
        identity: PricingIdentity {
            model: "openai/gpt-6.1-sol",
            service_tier: "default",
            version: "2026-09-30",
            source: "https://developers.openai.com/api/docs/pricing",
        },
        long_context_threshold: 272_000,
        long_context_threshold_display: "272k",
        short_context: Rates {
            uncached_input_nanos: 2_000,
            cached_input_nanos: 100,
            cache_write_nanos: 2_500,
            cache_write_1h_nanos: 2_500,
            output_nanos: 10_000,
        },
        long_context: Rates {
            uncached_input_nanos: 4_000,
            cached_input_nanos: 200,
            cache_write_nanos: 5_000,
            cache_write_1h_nanos: 5_000,
            output_nanos: 15_000,
        },
    },
};

pub static GPT6_SOL: ModelConfiguration = ModelConfiguration {
    id: "openai/gpt-6-sol",
    thinking_levels: THINKING_LEVELS,
    default_thinking: "medium",
    service_tier: "default",
    context_window_tokens: 1_050_000,
    context_window_display: "1.05m",
    max_output_tokens: 128_000,
    max_output_display: "128k",
    pricing: ModelPricing {
        identity: PricingIdentity {
            model: "openai/gpt-6-sol",
            service_tier: "default",
            version: "2026-09-23",
            source: "https://developers.openai.com/api/docs/pricing",
        },
        long_context_threshold: 272_000,
        long_context_threshold_display: "272k",
        short_context: Rates {
            uncached_input_nanos: 2_000,
            cached_input_nanos: 200,
            cache_write_nanos: 2_500,
            cache_write_1h_nanos: 2_500,
            output_nanos: 10_000,
        },
        long_context: Rates {
            uncached_input_nanos: 4_000,
            cached_input_nanos: 400,
            cache_write_nanos: 5_000,
            cache_write_1h_nanos: 5_000,
            output_nanos: 15_000,
        },
    },
};

pub static GPT6_LUNA: ModelConfiguration = ModelConfiguration {
    id: "openai/gpt-6-luna",
    thinking_levels: THINKING_LEVELS,
    default_thinking: "medium",
    service_tier: "default",
    context_window_tokens: 1_050_000,
    context_window_display: "1.05m",
    max_output_tokens: 128_000,
    max_output_display: "128k",
    pricing: ModelPricing {
        identity: PricingIdentity {
            model: "openai/gpt-6-luna",
            service_tier: "default",
            version: "2026-09-23",
            source: "https://developers.openai.com/api/docs/pricing",
        },
        long_context_threshold: 272_000,
        long_context_threshold_display: "272k",
        short_context: Rates {
            uncached_input_nanos: 100,
            cached_input_nanos: 10,
            cache_write_nanos: 125,
            cache_write_1h_nanos: 125,
            output_nanos: 500,
        },
        long_context: Rates {
            uncached_input_nanos: 200,
            cached_input_nanos: 20,
            cache_write_nanos: 250,
            cache_write_1h_nanos: 250,
            output_nanos: 750,
        },
    },
};

pub static SOL: ModelConfiguration = ModelConfiguration {
    id: "openai/gpt-5.6-sol",
    thinking_levels: THINKING_LEVELS,
    default_thinking: "medium",
    service_tier: "default",
    context_window_tokens: 1_050_000,
    context_window_display: "1.05m",
    max_output_tokens: 128_000,
    max_output_display: "128k",
    pricing: ModelPricing {
        identity: PricingIdentity {
            model: "openai/gpt-5.6-sol",
            service_tier: "default",
            version: "2026-08-27",
            source: "https://developers.openai.com/api/docs/pricing",
        },
        long_context_threshold: 272_000,
        long_context_threshold_display: "272k",
        short_context: Rates {
            uncached_input_nanos: 4_000,
            cached_input_nanos: 400,
            cache_write_nanos: 5_000,
            cache_write_1h_nanos: 5_000,
            output_nanos: 20_000,
        },
        long_context: Rates {
            uncached_input_nanos: 8_000,
            cached_input_nanos: 800,
            cache_write_nanos: 10_000,
            cache_write_1h_nanos: 10_000,
            output_nanos: 30_000,
        },
    },
};

pub static TERRA: ModelConfiguration = ModelConfiguration {
    id: "openai/gpt-5.6-terra",
    thinking_levels: THINKING_LEVELS,
    default_thinking: "medium",
    service_tier: "default",
    context_window_tokens: 1_050_000,
    context_window_display: "1.05m",
    max_output_tokens: 128_000,
    max_output_display: "128k",
    pricing: ModelPricing {
        identity: PricingIdentity {
            model: "openai/gpt-5.6-terra",
            service_tier: "default",
            version: "2026-08-27",
            source: "https://developers.openai.com/api/docs/pricing",
        },
        long_context_threshold: 272_000,
        long_context_threshold_display: "272k",
        short_context: Rates {
            uncached_input_nanos: 2_000,
            cached_input_nanos: 200,
            cache_write_nanos: 2_500,
            cache_write_1h_nanos: 2_500,
            output_nanos: 12_000,
        },
        long_context: Rates {
            uncached_input_nanos: 4_000,
            cached_input_nanos: 400,
            cache_write_nanos: 5_000,
            cache_write_1h_nanos: 5_000,
            output_nanos: 18_000,
        },
    },
};

pub static LUNA: ModelConfiguration = ModelConfiguration {
    id: "openai/gpt-5.6-luna",
    thinking_levels: THINKING_LEVELS,
    default_thinking: "medium",
    service_tier: "default",
    context_window_tokens: 1_050_000,
    context_window_display: "1.05m",
    max_output_tokens: 128_000,
    max_output_display: "128k",
    pricing: ModelPricing {
        identity: PricingIdentity {
            model: "openai/gpt-5.6-luna",
            service_tier: "default",
            version: "2026-08-27",
            source: "https://developers.openai.com/api/docs/pricing",
        },
        long_context_threshold: 272_000,
        long_context_threshold_display: "272k",
        short_context: Rates {
            uncached_input_nanos: 200,
            cached_input_nanos: 20,
            cache_write_nanos: 250,
            cache_write_1h_nanos: 250,
            output_nanos: 1_200,
        },
        long_context: Rates {
            uncached_input_nanos: 400,
            cached_input_nanos: 40,
            cache_write_nanos: 500,
            cache_write_1h_nanos: 500,
            output_nanos: 1_800,
        },
    },
};

pub static OPUS_5_5: ModelConfiguration = ModelConfiguration {
    id: "anthropic/claude-opus-5-5",
    thinking_levels: THINKING_LEVELS,
    default_thinking: "medium",
    service_tier: "standard",
    context_window_tokens: 1_000_000,
    context_window_display: "1m",
    max_output_tokens: 128_000,
    max_output_display: "128k",
    pricing: ModelPricing {
        identity: PricingIdentity {
            model: "anthropic/claude-opus-5-5",
            service_tier: "standard",
            version: "2026-09-23",
            source: "https://docs.anthropic.com/en/docs/about-claude/pricing",
        },
        long_context_threshold: 1_000_000,
        long_context_threshold_display: "1m",
        short_context: Rates {
            uncached_input_nanos: 4_000,
            cached_input_nanos: 200,
            cache_write_nanos: 5_000,
            cache_write_1h_nanos: 8_000,
            output_nanos: 20_000,
        },
        long_context: Rates {
            uncached_input_nanos: 4_000,
            cached_input_nanos: 200,
            cache_write_nanos: 5_000,
            cache_write_1h_nanos: 8_000,
            output_nanos: 20_000,
        },
    },
};

pub static SONNET_5_5: ModelConfiguration = ModelConfiguration {
    id: "anthropic/claude-sonnet-5-5",
    thinking_levels: THINKING_LEVELS,
    default_thinking: "high",
    service_tier: "standard",
    context_window_tokens: 1_000_000,
    context_window_display: "1m",
    max_output_tokens: 128_000,
    max_output_display: "128k",
    pricing: ModelPricing {
        identity: PricingIdentity {
            model: "anthropic/claude-sonnet-5-5",
            service_tier: "standard",
            version: "2026-09-30",
            source: "https://platform.claude.com/docs/en/about-claude/pricing",
        },
        long_context_threshold: 1_000_000,
        long_context_threshold_display: "1m",
        short_context: Rates {
            uncached_input_nanos: 2_000,
            cached_input_nanos: 200,
            cache_write_nanos: 2_500,
            cache_write_1h_nanos: 4_000,
            output_nanos: 10_000,
        },
        long_context: Rates {
            uncached_input_nanos: 2_000,
            cached_input_nanos: 200,
            cache_write_nanos: 2_500,
            cache_write_1h_nanos: 4_000,
            output_nanos: 10_000,
        },
    },
};

pub static FABLE_5_1: ModelConfiguration = ModelConfiguration {
    id: "anthropic/claude-fable-5-1",
    thinking_levels: THINKING_LEVELS,
    default_thinking: "high",
    service_tier: "standard",
    context_window_tokens: 1_000_000,
    context_window_display: "1m",
    max_output_tokens: 128_000,
    max_output_display: "128k",
    pricing: ModelPricing {
        identity: PricingIdentity {
            model: "anthropic/claude-fable-5-1",
            service_tier: "standard",
            version: "2026-09-23",
            source: "https://docs.anthropic.com/en/docs/about-claude/pricing",
        },
        long_context_threshold: 1_000_000,
        long_context_threshold_display: "1m",
        short_context: Rates {
            uncached_input_nanos: 10_000,
            cached_input_nanos: 250,
            cache_write_nanos: 12_500,
            cache_write_1h_nanos: 20_000,
            output_nanos: 50_000,
        },
        long_context: Rates {
            uncached_input_nanos: 10_000,
            cached_input_nanos: 250,
            cache_write_nanos: 12_500,
            cache_write_1h_nanos: 20_000,
            output_nanos: 50_000,
        },
    },
};

pub static GEMINI_4_ARGON: ModelConfiguration = ModelConfiguration {
    id: "google/gemini-4-argon",
    thinking_levels: GOOGLE_THINKING_LEVELS,
    default_thinking: "medium",
    service_tier: "standard",
    context_window_tokens: 1_048_576,
    context_window_display: "1m",
    max_output_tokens: 65_536,
    max_output_display: "64k",
    pricing: ModelPricing {
        identity: PricingIdentity {
            model: "google/gemini-4-argon",
            service_tier: "standard",
            version: "2026-10-14",
            source: "https://ai.google.dev/gemini-api/docs/pricing",
        },
        long_context_threshold: 1_048_576,
        long_context_threshold_display: "1m",
        short_context: Rates {
            uncached_input_nanos: 2_000,
            cached_input_nanos: 100,
            cache_write_nanos: 0,
            cache_write_1h_nanos: 0,
            output_nanos: 10_000,
        },
        long_context: Rates {
            uncached_input_nanos: 2_000,
            cached_input_nanos: 100,
            cache_write_nanos: 0,
            cache_write_1h_nanos: 0,
            output_nanos: 10_000,
        },
    },
};

pub static GEMINI_3_8_FLASH: ModelConfiguration = ModelConfiguration {
    id: "google/gemini-3.8-flash",
    thinking_levels: GOOGLE_THINKING_LEVELS,
    default_thinking: "medium",
    service_tier: "standard",
    context_window_tokens: 1_048_576,
    context_window_display: "1m",
    max_output_tokens: 65_536,
    max_output_display: "64k",
    pricing: ModelPricing {
        identity: PricingIdentity {
            model: "google/gemini-3.8-flash",
            service_tier: "standard",
            version: "2026-10-14",
            source: "https://ai.google.dev/gemini-api/docs/pricing",
        },
        long_context_threshold: 1_048_576,
        long_context_threshold_display: "1m",
        short_context: Rates {
            uncached_input_nanos: 750,
            cached_input_nanos: 75,
            cache_write_nanos: 0,
            cache_write_1h_nanos: 0,
            output_nanos: 3_750,
        },
        long_context: Rates {
            uncached_input_nanos: 750,
            cached_input_nanos: 75,
            cache_write_nanos: 0,
            cache_write_1h_nanos: 0,
            output_nanos: 3_750,
        },
    },
};

pub static CATALOGUE: [&ModelConfiguration; 12] = [
    &ASTRA,
    &GPT6_1_SOL,
    &GPT6_SOL,
    &GPT6_LUNA,
    &SOL,
    &TERRA,
    &LUNA,
    &OPUS_5_5,
    &SONNET_5_5,
    &FABLE_5_1,
    &GEMINI_4_ARGON,
    &GEMINI_3_8_FLASH,
];
pub static DEFAULT_MODEL: &ModelConfiguration = &SOL;

impl ModelConfiguration {
    pub fn supports_thinking(&self, thinking: &str) -> bool {
        self.thinking_levels.contains(&thinking)
    }

    pub fn provider(&self) -> Provider {
        self.qualified_id().0
    }

    pub fn wire_id(&self) -> &'static str {
        self.qualified_id().1
    }

    fn qualified_id(&self) -> (Provider, &'static str) {
        match self.id.split_once('/') {
            Some(("openai", wire_id)) => (Provider::OpenAi, wire_id),
            Some(("anthropic", wire_id)) => (Provider::Anthropic, wire_id),
            Some(("google", wire_id)) => (Provider::Google, wire_id),
            _ => panic!("model id `{}` has no known provider prefix", self.id),
        }
    }
}

pub fn model_by_id(id: &str) -> Option<&'static ModelConfiguration> {
    CATALOGUE.iter().copied().find(|model| model.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn catalogue_identity_order_and_default_are_exact() {
        assert_eq!(
            CATALOGUE.iter().map(|m| m.id).collect::<Vec<_>>(),
            [
                "openai/gpt-6-astra",
                "openai/gpt-6.1-sol",
                "openai/gpt-6-sol",
                "openai/gpt-6-luna",
                "openai/gpt-5.6-sol",
                "openai/gpt-5.6-terra",
                "openai/gpt-5.6-luna",
                "anthropic/claude-opus-5-5",
                "anthropic/claude-sonnet-5-5",
                "anthropic/claude-fable-5-1",
                "google/gemini-4-argon",
                "google/gemini-3.8-flash"
            ]
        );
        assert_eq!(
            CATALOGUE.iter().map(|m| m.id).collect::<HashSet<_>>().len(),
            12
        );
        assert!(std::ptr::eq(DEFAULT_MODEL, &SOL));
    }

    #[test]
    fn provider_and_wire_id_derive_from_the_qualified_id_prefix() {
        assert_eq!(
            CATALOGUE
                .iter()
                .map(|m| (m.id, m.provider(), m.wire_id()))
                .collect::<Vec<_>>(),
            [
                ("openai/gpt-6-astra", Provider::OpenAi, "gpt-6-astra"),
                ("openai/gpt-6.1-sol", Provider::OpenAi, "gpt-6.1-sol"),
                ("openai/gpt-6-sol", Provider::OpenAi, "gpt-6-sol"),
                ("openai/gpt-6-luna", Provider::OpenAi, "gpt-6-luna"),
                ("openai/gpt-5.6-sol", Provider::OpenAi, "gpt-5.6-sol"),
                ("openai/gpt-5.6-terra", Provider::OpenAi, "gpt-5.6-terra"),
                ("openai/gpt-5.6-luna", Provider::OpenAi, "gpt-5.6-luna"),
                (
                    "anthropic/claude-opus-5-5",
                    Provider::Anthropic,
                    "claude-opus-5-5"
                ),
                (
                    "anthropic/claude-sonnet-5-5",
                    Provider::Anthropic,
                    "claude-sonnet-5-5"
                ),
                (
                    "anthropic/claude-fable-5-1",
                    Provider::Anthropic,
                    "claude-fable-5-1"
                ),
                ("google/gemini-4-argon", Provider::Google, "gemini-4-argon"),
                (
                    "google/gemini-3.8-flash",
                    Provider::Google,
                    "gemini-3.8-flash"
                ),
            ]
        );
        assert_eq!(
            (
                Provider::OpenAi.display_name(),
                Provider::OpenAi.api_key_variable()
            ),
            ("OpenAI", "OPENAI_API_KEY")
        );
        assert_eq!(
            (
                Provider::Anthropic.display_name(),
                Provider::Anthropic.api_key_variable()
            ),
            ("Anthropic", "ANTHROPIC_API_KEY")
        );
        assert_eq!(
            (
                Provider::Google.display_name(),
                Provider::Google.api_key_variable()
            ),
            ("Google", "GEMINI_API_KEY")
        );
    }

    #[test]
    fn anthropic_models_match_the_spec_catalogue_and_pricing() {
        for (id, default_thinking, rates) in [
            (
                "anthropic/claude-opus-5-5",
                "medium",
                Rates {
                    uncached_input_nanos: 4_000,
                    cached_input_nanos: 200,
                    cache_write_nanos: 5_000,
                    cache_write_1h_nanos: 8_000,
                    output_nanos: 20_000,
                },
            ),
            (
                "anthropic/claude-fable-5-1",
                "high",
                Rates {
                    uncached_input_nanos: 10_000,
                    cached_input_nanos: 250,
                    cache_write_nanos: 12_500,
                    cache_write_1h_nanos: 20_000,
                    output_nanos: 50_000,
                },
            ),
        ] {
            let model = model_by_id(id).expect("Anthropic model is in the catalogue");
            assert!(std::ptr::eq(model.thinking_levels, THINKING_LEVELS));
            assert_eq!(model.default_thinking, default_thinking);
            assert_eq!(model.service_tier, "standard");
            assert_eq!(
                (model.context_window_tokens, model.context_window_display),
                (1_000_000, "1m")
            );
            assert_eq!(
                (model.max_output_tokens, model.max_output_display),
                (128_000, "128k")
            );
            assert_eq!(
                model.pricing.identity,
                PricingIdentity {
                    model: id,
                    service_tier: "standard",
                    version: "2026-09-23",
                    source: "https://docs.anthropic.com/en/docs/about-claude/pricing",
                }
            );
            assert_eq!(
                (
                    model.pricing.long_context_threshold,
                    model.pricing.long_context_threshold_display
                ),
                (1_000_000, "1m")
            );
            assert_eq!(model.pricing.short_context, rates);
            assert_eq!(model.pricing.long_context, rates);
        }
    }

    #[test]
    fn gpt_6_1_sol_and_sonnet_5_5_match_published_metadata_and_rates() {
        for (id, default_thinking, source, short_context, long_context) in [
            (
                "openai/gpt-6.1-sol",
                "medium",
                "https://developers.openai.com/api/docs/pricing",
                Rates {
                    uncached_input_nanos: 2_000,
                    cached_input_nanos: 100,
                    cache_write_nanos: 2_500,
                    cache_write_1h_nanos: 2_500,
                    output_nanos: 10_000,
                },
                Rates {
                    uncached_input_nanos: 4_000,
                    cached_input_nanos: 200,
                    cache_write_nanos: 5_000,
                    cache_write_1h_nanos: 5_000,
                    output_nanos: 15_000,
                },
            ),
            (
                "anthropic/claude-sonnet-5-5",
                "high",
                "https://platform.claude.com/docs/en/about-claude/pricing",
                Rates {
                    uncached_input_nanos: 2_000,
                    cached_input_nanos: 200,
                    cache_write_nanos: 2_500,
                    cache_write_1h_nanos: 4_000,
                    output_nanos: 10_000,
                },
                Rates {
                    uncached_input_nanos: 2_000,
                    cached_input_nanos: 200,
                    cache_write_nanos: 2_500,
                    cache_write_1h_nanos: 4_000,
                    output_nanos: 10_000,
                },
            ),
        ] {
            let model = model_by_id(id).expect("new model is in the catalogue");
            assert_eq!(model.default_thinking, default_thinking);
            assert_eq!(model.pricing.identity.version, "2026-09-30");
            assert_eq!(model.pricing.identity.source, source);
            assert_eq!(model.pricing.short_context, short_context);
            assert_eq!(model.pricing.long_context, long_context);
            for &thinking in THINKING_LEVELS {
                assert_eq!(
                    ModelSelection::new(model, thinking).unwrap().thinking(),
                    thinking
                );
            }
        }
        let sonnet = model_by_id("anthropic/claude-sonnet-5-5").unwrap();
        assert_eq!(
            (sonnet.context_window_tokens, sonnet.context_window_display),
            (1_000_000, "1m")
        );
    }

    #[test]
    fn gpt_6_sol_and_luna_match_published_standard_rates() {
        for (model, short, long) in [
            (
                &GPT6_SOL,
                Rates {
                    uncached_input_nanos: 2_000,
                    cached_input_nanos: 200,
                    cache_write_nanos: 2_500,
                    cache_write_1h_nanos: 2_500,
                    output_nanos: 10_000,
                },
                Rates {
                    uncached_input_nanos: 4_000,
                    cached_input_nanos: 400,
                    cache_write_nanos: 5_000,
                    cache_write_1h_nanos: 5_000,
                    output_nanos: 15_000,
                },
            ),
            (
                &GPT6_LUNA,
                Rates {
                    uncached_input_nanos: 100,
                    cached_input_nanos: 10,
                    cache_write_nanos: 125,
                    cache_write_1h_nanos: 125,
                    output_nanos: 500,
                },
                Rates {
                    uncached_input_nanos: 200,
                    cached_input_nanos: 20,
                    cache_write_nanos: 250,
                    cache_write_1h_nanos: 250,
                    output_nanos: 750,
                },
            ),
        ] {
            assert_eq!(model.default_thinking, "medium");
            assert_eq!(model.pricing.short_context, short);
            assert_eq!(model.pricing.long_context, long);
            assert_eq!(
                model.pricing.identity.source,
                "https://developers.openai.com/api/docs/pricing"
            );
            assert!(std::ptr::eq(model_by_id(model.id).unwrap(), model));
        }
    }

    #[test]
    fn google_models_match_the_spec_catalogue_and_pricing() {
        for (model, id, wire_id, rates) in [
            (
                &GEMINI_4_ARGON,
                "google/gemini-4-argon",
                "gemini-4-argon",
                Rates {
                    uncached_input_nanos: 2_000,
                    cached_input_nanos: 100,
                    cache_write_nanos: 0,
                    cache_write_1h_nanos: 0,
                    output_nanos: 10_000,
                },
            ),
            (
                &GEMINI_3_8_FLASH,
                "google/gemini-3.8-flash",
                "gemini-3.8-flash",
                Rates {
                    uncached_input_nanos: 750,
                    cached_input_nanos: 75,
                    cache_write_nanos: 0,
                    cache_write_1h_nanos: 0,
                    output_nanos: 3_750,
                },
            ),
        ] {
            assert_eq!(model.id, id);
            assert_eq!(model.provider(), Provider::Google);
            assert_eq!(model.wire_id(), wire_id);
            assert!(std::ptr::eq(model_by_id(id).unwrap(), model));
            assert!(std::ptr::eq(model.thinking_levels, GOOGLE_THINKING_LEVELS));
            assert_eq!(model.thinking_levels, &["low", "medium", "high"]);
            assert_eq!(model.default_thinking, "medium");
            for &thinking in GOOGLE_THINKING_LEVELS {
                assert!(model.supports_thinking(thinking));
                assert_eq!(
                    ModelSelection::new(model, thinking).unwrap().thinking(),
                    thinking
                );
            }
            for unsupported in ["none", "minimal", "xhigh", "max"] {
                assert!(!model.supports_thinking(unsupported));
                assert_eq!(ModelSelection::new(model, unsupported), None);
            }
            assert_eq!(model.service_tier, "standard");
            assert_eq!(
                (model.context_window_tokens, model.context_window_display),
                (1_048_576, "1m")
            );
            assert_eq!(
                (model.max_output_tokens, model.max_output_display),
                (65_536, "64k")
            );
            assert_eq!(
                model.pricing.identity,
                PricingIdentity {
                    model: id,
                    service_tier: "standard",
                    version: "2026-10-14",
                    source: "https://ai.google.dev/gemini-api/docs/pricing",
                }
            );
            assert_eq!(
                (
                    model.pricing.long_context_threshold,
                    model.pricing.long_context_threshold_display
                ),
                (1_048_576, "1m")
            );
            assert_eq!(model.pricing.short_context, rates);
            assert_eq!(model.pricing.long_context, rates);
        }
    }

    #[test]
    fn universal_capacities_and_identity_invariants_hold() {
        for model in CATALOGUE {
            assert!(!model.supports_thinking("none"));
            assert!(model.thinking_levels.contains(&model.default_thinking));
            assert_eq!(model.pricing.identity.model, model.id);
            assert_eq!(model.pricing.identity.service_tier, model.service_tier);
            if model.provider() != Provider::Google {
                assert!(std::ptr::eq(model.thinking_levels, THINKING_LEVELS));
                assert_eq!(
                    model.thinking_levels,
                    &["low", "medium", "high", "xhigh", "max"]
                );
                assert_eq!(
                    (model.max_output_tokens, model.max_output_display),
                    (128_000, "128k")
                );
            }
            match model.provider() {
                Provider::OpenAi => {
                    assert_eq!(model.service_tier, "default");
                    assert_eq!(
                        (model.context_window_tokens, model.context_window_display),
                        (1_050_000, "1.05m")
                    );
                    assert_eq!(
                        (
                            model.pricing.long_context_threshold,
                            model.pricing.long_context_threshold_display
                        ),
                        (272_000, "272k")
                    );
                }
                Provider::Anthropic | Provider::Google => {
                    assert_eq!(model.service_tier, "standard");
                    assert_eq!(
                        (
                            model.pricing.long_context_threshold,
                            model.pricing.long_context_threshold_display
                        ),
                        (model.context_window_tokens, model.context_window_display)
                    );
                    assert_eq!(model.pricing.long_context, model.pricing.short_context);
                }
            }
        }
    }

    #[test]
    fn rates_match_the_3_september_2026_research_record() {
        let expected = [
            (
                &SOL,
                Rates {
                    uncached_input_nanos: 4_000,
                    cached_input_nanos: 400,
                    cache_write_nanos: 5_000,
                    cache_write_1h_nanos: 5_000,
                    output_nanos: 20_000,
                },
                Rates {
                    uncached_input_nanos: 8_000,
                    cached_input_nanos: 800,
                    cache_write_nanos: 10_000,
                    cache_write_1h_nanos: 10_000,
                    output_nanos: 30_000,
                },
            ),
            (
                &TERRA,
                Rates {
                    uncached_input_nanos: 2_000,
                    cached_input_nanos: 200,
                    cache_write_nanos: 2_500,
                    cache_write_1h_nanos: 2_500,
                    output_nanos: 12_000,
                },
                Rates {
                    uncached_input_nanos: 4_000,
                    cached_input_nanos: 400,
                    cache_write_nanos: 5_000,
                    cache_write_1h_nanos: 5_000,
                    output_nanos: 18_000,
                },
            ),
            (
                &LUNA,
                Rates {
                    uncached_input_nanos: 200,
                    cached_input_nanos: 20,
                    cache_write_nanos: 250,
                    cache_write_1h_nanos: 250,
                    output_nanos: 1_200,
                },
                Rates {
                    uncached_input_nanos: 400,
                    cached_input_nanos: 40,
                    cache_write_nanos: 500,
                    cache_write_1h_nanos: 500,
                    output_nanos: 1_800,
                },
            ),
        ];

        for (model, short_context, long_context) in expected {
            assert_eq!(model.pricing.short_context, short_context);
            assert_eq!(model.pricing.long_context, long_context);
        }
    }

    #[test]
    fn astra_metadata_thinking_and_rates_are_exact() {
        assert_eq!(
            ASTRA.thinking_levels,
            &["low", "medium", "high", "xhigh", "max"]
        );
        assert!(std::ptr::eq(ASTRA.thinking_levels, THINKING_LEVELS));
        assert_eq!(ASTRA.default_thinking, "low");
        assert!(!ASTRA.supports_thinking("none"));
        assert_eq!(ASTRA.pricing.identity.version, "2026-09-04");
        assert_eq!(
            ASTRA.pricing.identity.source,
            "https://openai.com/api/pricing/"
        );
        assert_eq!(
            ASTRA.pricing.short_context,
            Rates {
                uncached_input_nanos: 10_000,
                cached_input_nanos: 1_000,
                cache_write_nanos: 12_500,
                cache_write_1h_nanos: 12_500,
                output_nanos: 50_000
            }
        );
        assert_eq!(
            ASTRA.pricing.long_context,
            Rates {
                uncached_input_nanos: 20_000,
                cached_input_nanos: 2_000,
                cache_write_nanos: 25_000,
                cache_write_1h_nanos: 25_000,
                output_nanos: 75_000
            }
        );
    }

    #[test]
    fn gpt_56_defaults_and_pricing_are_unchanged() {
        for model in [&SOL, &TERRA, &LUNA] {
            assert!(std::ptr::eq(model.thinking_levels, THINKING_LEVELS));
            assert_eq!(
                model.thinking_levels,
                &["low", "medium", "high", "xhigh", "max"]
            );
            assert_eq!(model.default_thinking, "medium");
            assert_eq!(model.pricing.identity.version, "2026-08-27");
            assert_eq!(
                model.pricing.identity.source,
                "https://developers.openai.com/api/docs/pricing"
            );
        }
    }

    #[test]
    fn selection_and_exact_lookup_obey_model_capabilities() {
        assert_eq!(ModelSelection::new(&ASTRA, "none"), None);
        assert_eq!(
            ModelSelection::new(&ASTRA, "low").unwrap().thinking(),
            "low"
        );
        for model in CATALOGUE {
            assert!(std::ptr::eq(model_by_id(model.id).unwrap(), model));
        }
        for id in [
            "gpt-6-astra",
            "openai/GPT-6-ASTRA",
            "openai/gpt-6-astra ",
            "openai/gpt-6-astra-2026-09-03",
            "unknown",
        ] {
            assert_eq!(model_by_id(id), None);
        }
    }
}
