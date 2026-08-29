use crate::model::{AttemptAccounting, ModelUsage};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PricingIdentity {
    pub model: &'static str,
    pub service_tier: &'static str,
    pub version: &'static str,
    pub source: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rates {
    uncached_input_nanos: u64,
    cached_input_nanos: u64,
    cache_write_nanos: u64,
    output_nanos: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelPricing {
    identity: PricingIdentity,
    long_context_threshold: u64,
    short_context: Rates,
    long_context: Rates,
}

impl ModelPricing {
    pub fn identity(&self) -> PricingIdentity {
        self.identity
    }
}

pub const GPT_5_6_SOL_DEFAULT_2026_08_27: ModelPricing = ModelPricing {
    identity: PricingIdentity {
        model: "gpt-5.6-sol",
        service_tier: "default",
        version: "2026-08-27",
        source: "https://developers.openai.com/api/docs/pricing",
    },
    long_context_threshold: 272_000,
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
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsdNanos(u128);

impl UsdNanos {
    pub fn as_nanos(self) -> u128 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CostEstimate {
    amount: UsdNanos,
    pricing: PricingIdentity,
}

impl CostEstimate {
    pub fn amount(&self) -> UsdNanos {
        self.amount
    }

    pub fn pricing(&self) -> PricingIdentity {
        self.pricing
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunCostEstimate {
    Complete(CostEstimate),
    LowerBound(CostEstimate),
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageCategory {
    Input,
    CachedInput,
    CacheWriteInput,
    Output,
    ReasoningOutput,
    Total,
}

#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq, Eq)]
pub enum PricingError {
    #[error("usage category `{0:?}` is missing")]
    MissingCategory(UsageCategory),
    #[error("usage model does not match the pricing model")]
    ModelMismatch,
    #[error("returned service tier does not match the pricing tier")]
    ServiceTierMismatch,
    #[error("cached and cache-write input exceeds total input")]
    InconsistentInputBreakdown,
    #[error("reasoning output exceeds total output")]
    InconsistentReasoningBreakdown,
    #[error("total tokens does not equal input plus output")]
    InconsistentTotal,
    #[error("usage cost arithmetic overflowed")]
    ArithmeticOverflow,
}

pub fn estimate_usage(usage: &ModelUsage) -> Result<CostEstimate, PricingError> {
    let input = usage
        .input_tokens
        .ok_or(PricingError::MissingCategory(UsageCategory::Input))?;
    let cached = usage
        .cached_input_tokens
        .ok_or(PricingError::MissingCategory(UsageCategory::CachedInput))?;
    let cache_write = usage
        .cache_write_input_tokens
        .ok_or(PricingError::MissingCategory(
            UsageCategory::CacheWriteInput,
        ))?;
    let output = usage
        .output_tokens
        .ok_or(PricingError::MissingCategory(UsageCategory::Output))?;
    let reasoning = usage
        .reasoning_output_tokens
        .ok_or(PricingError::MissingCategory(
            UsageCategory::ReasoningOutput,
        ))?;
    let total = usage
        .total_tokens
        .ok_or(PricingError::MissingCategory(UsageCategory::Total))?;

    let pricing = GPT_5_6_SOL_DEFAULT_2026_08_27;
    if usage.model != pricing.identity.model {
        return Err(PricingError::ModelMismatch);
    }
    if usage.service_tier.as_deref() != Some(pricing.identity.service_tier) {
        return Err(PricingError::ServiceTierMismatch);
    }

    let categorised_input = cached
        .checked_add(cache_write)
        .ok_or(PricingError::ArithmeticOverflow)?;
    if categorised_input > input {
        return Err(PricingError::InconsistentInputBreakdown);
    }
    if reasoning > output {
        return Err(PricingError::InconsistentReasoningBreakdown);
    }
    if input
        .checked_add(output)
        .ok_or(PricingError::ArithmeticOverflow)?
        != total
    {
        return Err(PricingError::InconsistentTotal);
    }
    let uncached = input
        .checked_sub(categorised_input)
        .ok_or(PricingError::ArithmeticOverflow)?;
    let rates = if input <= pricing.long_context_threshold {
        pricing.short_context
    } else {
        pricing.long_context
    };
    let amount = [
        (uncached, rates.uncached_input_nanos),
        (cached, rates.cached_input_nanos),
        (cache_write, rates.cache_write_nanos),
        (output, rates.output_nanos),
    ]
    .into_iter()
    .try_fold(0_u128, |total, (tokens, rate)| {
        let category_cost = u128::from(tokens)
            .checked_mul(u128::from(rate))
            .ok_or(PricingError::ArithmeticOverflow)?;
        total
            .checked_add(category_cost)
            .ok_or(PricingError::ArithmeticOverflow)
    })?;

    Ok(CostEstimate {
        amount: UsdNanos(amount),
        pricing: pricing.identity,
    })
}

pub fn estimate_run_cost<'a>(
    attempts: impl IntoIterator<Item = &'a AttemptAccounting>,
) -> RunCostEstimate {
    let mut known_amount = 0_u128;
    let mut has_known = false;
    let mut is_lower_bound = false;
    let mut has_unpriceable = false;

    for attempt in attempts {
        let usage = match attempt {
            AttemptAccounting::KnownUsage(usage) => usage,
            AttemptAccounting::KnownUsageWithUntrackedRetries(usage) => {
                is_lower_bound = true;
                usage
            }
            AttemptAccounting::UsageUnavailable => {
                has_unpriceable = true;
                continue;
            }
        };
        let Ok(estimate) = estimate_usage(usage) else {
            has_unpriceable = true;
            continue;
        };
        let Some(total) = known_amount.checked_add(estimate.amount.0) else {
            has_unpriceable = true;
            continue;
        };
        known_amount = total;
        has_known = true;
    }

    let estimate = CostEstimate {
        amount: UsdNanos(known_amount),
        pricing: GPT_5_6_SOL_DEFAULT_2026_08_27.identity,
    };
    match (has_known, has_unpriceable || is_lower_bound) {
        (true, true) => RunCostEstimate::LowerBound(estimate),
        (true, false) | (false, false) => RunCostEstimate::Complete(estimate),
        (false, true) => RunCostEstimate::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CostEstimate, GPT_5_6_SOL_DEFAULT_2026_08_27, PricingError, RunCostEstimate, UsageCategory,
        estimate_run_cost, estimate_usage,
    };
    use crate::model::{AttemptAccounting, ModelUsage};

    fn usage(input: u64, cached: u64, cache_write: u64, output: u64, reasoning: u64) -> ModelUsage {
        ModelUsage {
            model: "gpt-5.6-sol".to_owned(),
            service_tier: Some("default".to_owned()),
            input_tokens: Some(input),
            cached_input_tokens: Some(cached),
            cache_write_input_tokens: Some(cache_write),
            output_tokens: Some(output),
            reasoning_output_tokens: Some(reasoning),
            total_tokens: Some(input + output),
        }
    }

    fn amount(estimate: CostEstimate) -> u128 {
        estimate.amount().as_nanos()
    }

    #[test]
    fn pricing_constant_has_versioned_official_identity() {
        let identity = GPT_5_6_SOL_DEFAULT_2026_08_27.identity();

        assert_eq!(identity.model, "gpt-5.6-sol");
        assert_eq!(identity.service_tier, "default");
        assert_eq!(identity.version, "2026-08-27");
        assert_eq!(
            identity.source,
            "https://developers.openai.com/api/docs/pricing"
        );
    }

    #[test]
    fn estimates_uncached_cached_cache_write_and_output_categories_exactly() {
        let estimate = estimate_usage(&usage(200_000, 40_000, 60_000, 100_000, 50_000)).unwrap();

        assert_eq!(amount(estimate), 2_716_000_000);
        assert_eq!(
            estimate.pricing(),
            GPT_5_6_SOL_DEFAULT_2026_08_27.identity()
        );
    }

    #[test]
    fn applies_short_rates_at_272_000_and_long_rates_to_all_categories_at_272_001() {
        let threshold = estimate_usage(&usage(272_000, 100_000, 100_000, 100_000, 0)).unwrap();
        let above = estimate_usage(&usage(272_001, 100_000, 100_000, 100_000, 0)).unwrap();

        assert_eq!(amount(threshold), 2_828_000_000);
        assert_eq!(amount(above), 4_656_008_000);
    }

    #[test]
    fn reasoning_tokens_are_not_charged_in_addition_to_output() {
        let without_reasoning = estimate_usage(&usage(0, 0, 0, 100_000, 0)).unwrap();
        let all_reasoning = estimate_usage(&usage(0, 0, 0, 100_000, 100_000)).unwrap();

        assert_eq!(amount(without_reasoning), 2_000_000_000);
        assert_eq!(all_reasoning, without_reasoning);
    }

    #[test]
    fn rejects_each_missing_usage_category() {
        let complete = usage(10, 2, 3, 4, 1);
        let cases = [
            (
                UsageCategory::Input,
                ModelUsage {
                    input_tokens: None,
                    ..complete.clone()
                },
            ),
            (
                UsageCategory::CachedInput,
                ModelUsage {
                    cached_input_tokens: None,
                    ..complete.clone()
                },
            ),
            (
                UsageCategory::CacheWriteInput,
                ModelUsage {
                    cache_write_input_tokens: None,
                    ..complete.clone()
                },
            ),
            (
                UsageCategory::Output,
                ModelUsage {
                    output_tokens: None,
                    ..complete.clone()
                },
            ),
            (
                UsageCategory::ReasoningOutput,
                ModelUsage {
                    reasoning_output_tokens: None,
                    ..complete.clone()
                },
            ),
            (
                UsageCategory::Total,
                ModelUsage {
                    total_tokens: None,
                    ..complete
                },
            ),
        ];

        for (category, incomplete) in cases {
            assert_eq!(
                estimate_usage(&incomplete),
                Err(PricingError::MissingCategory(category))
            );
        }
    }

    #[test]
    fn rejects_inconsistent_usage_and_checks_token_arithmetic() {
        assert_eq!(
            estimate_usage(&usage(10, 8, 3, 0, 0)),
            Err(PricingError::InconsistentInputBreakdown)
        );
        assert_eq!(
            estimate_usage(&usage(0, 0, 0, 10, 11)),
            Err(PricingError::InconsistentReasoningBreakdown)
        );

        let mut wrong_total = usage(10, 0, 0, 5, 0);
        wrong_total.total_tokens = Some(14);
        assert_eq!(
            estimate_usage(&wrong_total),
            Err(PricingError::InconsistentTotal)
        );

        let category_overflow = ModelUsage {
            cached_input_tokens: Some(u64::MAX),
            cache_write_input_tokens: Some(1),
            ..usage(u64::MAX, 0, 0, 0, 0)
        };
        assert_eq!(
            estimate_usage(&category_overflow),
            Err(PricingError::ArithmeticOverflow)
        );

        let total_overflow = ModelUsage {
            input_tokens: Some(u64::MAX),
            output_tokens: Some(1),
            total_tokens: Some(u64::MAX),
            ..usage(0, 0, 0, 0, 0)
        };
        assert_eq!(
            estimate_usage(&total_overflow),
            Err(PricingError::ArithmeticOverflow)
        );
    }

    #[test]
    fn rejects_mismatched_model_and_non_default_or_missing_returned_tier() {
        let mut wrong_model = usage(10, 0, 0, 0, 0);
        wrong_model.model = "gpt-5.6-terra".to_owned();
        assert_eq!(
            estimate_usage(&wrong_model),
            Err(PricingError::ModelMismatch)
        );

        for tier in [
            Some("priority".to_owned()),
            Some("Default".to_owned()),
            None,
        ] {
            let mut wrong_tier = usage(10, 0, 0, 0, 0);
            wrong_tier.service_tier = tier;
            assert_eq!(
                estimate_usage(&wrong_tier),
                Err(PricingError::ServiceTierMismatch)
            );
        }
    }

    #[test]
    fn run_cost_is_complete_only_when_every_attempt_is_priceable() {
        let attempts = [
            AttemptAccounting::KnownUsage(usage(200_000, 0, 0, 0, 0)),
            AttemptAccounting::KnownUsage(usage(0, 0, 0, 1_000_000, 0)),
        ];

        match estimate_run_cost(&attempts) {
            RunCostEstimate::Complete(estimate) => {
                assert_eq!(amount(estimate), 20_800_000_000);
                assert_eq!(
                    estimate.pricing(),
                    GPT_5_6_SOL_DEFAULT_2026_08_27.identity()
                );
            }
            estimate => panic!("expected complete estimate, got {estimate:?}"),
        }
    }

    #[test]
    fn run_cost_is_unavailable_instead_of_zero_when_no_attempt_is_priceable() {
        let mut invalid = usage(10, 0, 0, 0, 0);
        invalid.service_tier = Some("priority".to_owned());

        assert_eq!(
            estimate_run_cost(&[AttemptAccounting::UsageUnavailable]),
            RunCostEstimate::Unavailable
        );
        assert_eq!(
            estimate_run_cost(&[AttemptAccounting::KnownUsage(invalid)]),
            RunCostEstimate::Unavailable
        );
    }

    #[test]
    fn known_usage_with_untracked_retries_makes_flattened_multi_step_cost_a_lower_bound() {
        let first_step = [AttemptAccounting::KnownUsageWithUntrackedRetries(usage(
            200_000, 0, 0, 0, 0,
        ))];
        let second_step = [AttemptAccounting::KnownUsageWithUntrackedRetries(usage(
            0, 0, 0, 1_000_000, 0,
        ))];

        match estimate_run_cost(
            [first_step.as_slice(), second_step.as_slice()]
                .into_iter()
                .flatten(),
        ) {
            RunCostEstimate::LowerBound(estimate) => assert_eq!(amount(estimate), 20_800_000_000),
            estimate => panic!("expected lower bound, got {estimate:?}"),
        }
    }

    #[test]
    fn known_estimate_before_unknown_or_unpriceable_attempt_is_a_lower_bound() {
        let known = AttemptAccounting::KnownUsage(usage(200_000, 0, 0, 0, 0));
        let mut invalid_usage = usage(10, 0, 0, 0, 0);
        invalid_usage.cached_input_tokens = None;

        for attempts in [
            vec![known.clone(), AttemptAccounting::UsageUnavailable],
            vec![known.clone(), AttemptAccounting::KnownUsage(invalid_usage)],
        ] {
            match estimate_run_cost(&attempts) {
                RunCostEstimate::LowerBound(estimate) => {
                    assert_eq!(amount(estimate), 800_000_000);
                }
                estimate => panic!("expected lower bound, got {estimate:?}"),
            }
        }
    }
}
