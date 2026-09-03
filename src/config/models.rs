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
    pub service_tier: &'static str,
    pub context_window_tokens: u64,
    pub context_window_display: &'static str,
    pub max_output_tokens: u64,
    pub max_output_display: &'static str,
    pub pricing: ModelPricing,
}

pub static SOL: ModelConfiguration = ModelConfiguration {
    id: "gpt-5.6-sol",
    service_tier: "default",
    context_window_tokens: 1_050_000,
    context_window_display: "1.05m",
    max_output_tokens: 128_000,
    max_output_display: "128k",
    pricing: ModelPricing {
        identity: PricingIdentity {
            model: "gpt-5.6-sol",
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
    id: "gpt-5.6-terra",
    service_tier: "default",
    context_window_tokens: 1_050_000,
    context_window_display: "1.05m",
    max_output_tokens: 128_000,
    max_output_display: "128k",
    pricing: ModelPricing {
        identity: PricingIdentity {
            model: "gpt-5.6-terra",
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
    id: "gpt-5.6-luna",
    service_tier: "default",
    context_window_tokens: 1_050_000,
    context_window_display: "1.05m",
    max_output_tokens: 128_000,
    max_output_display: "128k",
    pricing: ModelPricing {
        identity: PricingIdentity {
            model: "gpt-5.6-luna",
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

pub static CATALOGUE: [&ModelConfiguration; 3] = [&SOL, &TERRA, &LUNA];
pub static DEFAULT_MODEL: &ModelConfiguration = &SOL;

pub fn model_by_id(id: &str) -> Option<&'static ModelConfiguration> {
    CATALOGUE.iter().copied().find(|model| model.id == id)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{CATALOGUE, DEFAULT_MODEL, LUNA, Rates, SOL, TERRA, model_by_id};

    #[test]
    fn catalogue_contains_each_supported_model_once_and_defaults_to_sol() {
        assert_eq!(CATALOGUE.len(), 3);
        assert_eq!(
            CATALOGUE.map(|model| model.id),
            ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"]
        );
        assert_eq!(
            CATALOGUE
                .iter()
                .map(|model| model.id)
                .collect::<HashSet<_>>()
                .len(),
            CATALOGUE.len()
        );
        assert!(std::ptr::eq(DEFAULT_MODEL, &SOL));
    }

    #[test]
    fn every_model_has_published_context_and_pricing_thresholds() {
        for model in CATALOGUE {
            assert_eq!(model.service_tier, "default");
            assert_eq!(model.context_window_tokens, 1_050_000);
            assert_eq!(model.context_window_display, "1.05m");
            assert_eq!(model.max_output_tokens, 128_000);
            assert_eq!(model.max_output_display, "128k");
            assert!(model.context_window_tokens >= model.max_output_tokens);
            assert_eq!(model.pricing.long_context_threshold, 272_000);
            assert_eq!(model.pricing.long_context_threshold_display, "272k");
            assert!(model.pricing.long_context_threshold <= model.context_window_tokens);
            assert_eq!(model.pricing.identity.model, model.id);
            assert_eq!(model.pricing.identity.service_tier, model.service_tier);
            assert_eq!(model.pricing.identity.version, "2026-08-27");
            assert_eq!(
                model.pricing.identity.source,
                "https://developers.openai.com/api/docs/pricing"
            );
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
    fn lookup_requires_an_exact_supported_model_id() {
        for model in [&SOL, &TERRA, &LUNA] {
            let found = model_by_id(model.id).expect("supported model should be found");
            assert!(std::ptr::eq(found, model));
        }

        for id in ["gpt-5.6", "GPT-5.6-SOL", "gpt-5.6-sol ", "unknown"] {
            assert_eq!(model_by_id(id), None);
        }
    }
}
