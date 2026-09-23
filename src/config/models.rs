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

pub static OPENAI_THINKING_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

pub static ASTRA: ModelConfiguration = ModelConfiguration {
    id: "openai/gpt-6-astra",
    thinking_levels: OPENAI_THINKING_LEVELS,
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
            output_nanos: 50_000,
        },
        long_context: Rates {
            uncached_input_nanos: 20_000,
            cached_input_nanos: 2_000,
            cache_write_nanos: 25_000,
            output_nanos: 75_000,
        },
    },
};

pub static GPT6_SOL: ModelConfiguration = ModelConfiguration {
    id: "openai/gpt-6-sol",
    thinking_levels: OPENAI_THINKING_LEVELS,
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
            output_nanos: 10_000,
        },
        long_context: Rates {
            uncached_input_nanos: 4_000,
            cached_input_nanos: 400,
            cache_write_nanos: 5_000,
            output_nanos: 15_000,
        },
    },
};

pub static GPT6_LUNA: ModelConfiguration = ModelConfiguration {
    id: "openai/gpt-6-luna",
    thinking_levels: OPENAI_THINKING_LEVELS,
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
            output_nanos: 500,
        },
        long_context: Rates {
            uncached_input_nanos: 200,
            cached_input_nanos: 20,
            cache_write_nanos: 250,
            output_nanos: 750,
        },
    },
};

pub static SOL: ModelConfiguration = ModelConfiguration {
    id: "openai/gpt-5.6-sol",
    thinking_levels: OPENAI_THINKING_LEVELS,
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
            output_nanos: 20_000,
        },
        long_context: Rates {
            uncached_input_nanos: 8_000,
            cached_input_nanos: 800,
            cache_write_nanos: 10_000,
            output_nanos: 30_000,
        },
    },
};

pub static TERRA: ModelConfiguration = ModelConfiguration {
    id: "openai/gpt-5.6-terra",
    thinking_levels: OPENAI_THINKING_LEVELS,
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
            output_nanos: 12_000,
        },
        long_context: Rates {
            uncached_input_nanos: 4_000,
            cached_input_nanos: 400,
            cache_write_nanos: 5_000,
            output_nanos: 18_000,
        },
    },
};

pub static LUNA: ModelConfiguration = ModelConfiguration {
    id: "openai/gpt-5.6-luna",
    thinking_levels: OPENAI_THINKING_LEVELS,
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
            output_nanos: 1_200,
        },
        long_context: Rates {
            uncached_input_nanos: 400,
            cached_input_nanos: 40,
            cache_write_nanos: 500,
            output_nanos: 1_800,
        },
    },
};

pub static CATALOGUE: [&ModelConfiguration; 6] =
    [&ASTRA, &GPT6_SOL, &GPT6_LUNA, &SOL, &TERRA, &LUNA];
pub static DEFAULT_MODEL: &ModelConfiguration = &SOL;

impl ModelConfiguration {
    pub fn supports_thinking(&self, thinking: &str) -> bool {
        self.thinking_levels.contains(&thinking)
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
            CATALOGUE.map(|m| m.id),
            [
                "openai/gpt-6-astra",
                "openai/gpt-6-sol",
                "openai/gpt-6-luna",
                "openai/gpt-5.6-sol",
                "openai/gpt-5.6-terra",
                "openai/gpt-5.6-luna"
            ]
        );
        assert_eq!(
            CATALOGUE.iter().map(|m| m.id).collect::<HashSet<_>>().len(),
            6
        );
        assert!(std::ptr::eq(DEFAULT_MODEL, &SOL));
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
                    output_nanos: 10_000,
                },
                Rates {
                    uncached_input_nanos: 4_000,
                    cached_input_nanos: 400,
                    cache_write_nanos: 5_000,
                    output_nanos: 15_000,
                },
            ),
            (
                &GPT6_LUNA,
                Rates {
                    uncached_input_nanos: 100,
                    cached_input_nanos: 10,
                    cache_write_nanos: 125,
                    output_nanos: 500,
                },
                Rates {
                    uncached_input_nanos: 200,
                    cached_input_nanos: 20,
                    cache_write_nanos: 250,
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
    fn universal_capacities_and_identity_invariants_hold() {
        for model in CATALOGUE {
            assert_eq!(model.service_tier, "default");
            assert!(std::ptr::eq(model.thinking_levels, OPENAI_THINKING_LEVELS));
            assert_eq!(
                model.thinking_levels,
                &["low", "medium", "high", "xhigh", "max"]
            );
            assert!(!model.supports_thinking("none"));
            assert!(model.thinking_levels.contains(&model.default_thinking));
            assert_eq!(
                (model.context_window_tokens, model.context_window_display),
                (1_050_000, "1.05m")
            );
            assert_eq!(
                (model.max_output_tokens, model.max_output_display),
                (128_000, "128k")
            );
            assert_eq!(
                (
                    model.pricing.long_context_threshold,
                    model.pricing.long_context_threshold_display
                ),
                (272_000, "272k")
            );
            assert_eq!(model.pricing.identity.model, model.id);
            assert_eq!(model.pricing.identity.service_tier, model.service_tier);
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
                    output_nanos: 20_000,
                },
                Rates {
                    uncached_input_nanos: 8_000,
                    cached_input_nanos: 800,
                    cache_write_nanos: 10_000,
                    output_nanos: 30_000,
                },
            ),
            (
                &TERRA,
                Rates {
                    uncached_input_nanos: 2_000,
                    cached_input_nanos: 200,
                    cache_write_nanos: 2_500,
                    output_nanos: 12_000,
                },
                Rates {
                    uncached_input_nanos: 4_000,
                    cached_input_nanos: 400,
                    cache_write_nanos: 5_000,
                    output_nanos: 18_000,
                },
            ),
            (
                &LUNA,
                Rates {
                    uncached_input_nanos: 200,
                    cached_input_nanos: 20,
                    cache_write_nanos: 250,
                    output_nanos: 1_200,
                },
                Rates {
                    uncached_input_nanos: 400,
                    cached_input_nanos: 40,
                    cache_write_nanos: 500,
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
        assert!(std::ptr::eq(ASTRA.thinking_levels, OPENAI_THINKING_LEVELS));
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
                output_nanos: 50_000
            }
        );
        assert_eq!(
            ASTRA.pricing.long_context,
            Rates {
                uncached_input_nanos: 20_000,
                cached_input_nanos: 2_000,
                cache_write_nanos: 25_000,
                output_nanos: 75_000
            }
        );
    }

    #[test]
    fn gpt_56_defaults_and_pricing_are_unchanged() {
        for model in [&SOL, &TERRA, &LUNA] {
            assert!(std::ptr::eq(model.thinking_levels, OPENAI_THINKING_LEVELS));
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
