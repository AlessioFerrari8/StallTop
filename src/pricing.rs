//! Built-in price table (USD x million tokens) and cost estimation
//! Costs are API-equivalent: on a sub plan they are not money actually spent

use crate::model::TokenUsage;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
    pub cache_read: f64,
}

/// Standard Anthropic multiplier: cache writes 1.25x (5min) and 2x (1 h), reads 0.1x
const fn anthropic(input: f64, output: f64) -> Price {
    Price {
        input,
        output,
        cache_write_5m: input * 1.25,
        cache_write_1h: input * 2.0,
        cache_read: input * 0.1,
    }
}

const fn with_read(p: Price, cache_read: f64) -> Price {
    Price { cache_read, ..p }
}

/// Gemini bills cached input at a discount and has no separate cache-write price
const fn gemini(input: f64, output: f64) -> Price {
    Price {
        input,
        output,
        cache_write_5m: input,
        cache_write_1h: input,
        cache_read: input * 0.1,
    }
}

/// OpenAI: cached input has its own price, cache writes cost like input
const fn openai(input: f64, cached: f64, output: f64) -> Price {
    Price {
        input,
        output,
        cache_write_5m: input,
        cache_write_1h: input,
        cache_read: cached,
    }
}

/// Matched by prefix, first match wins: more specific ids must come first. Anthropic prices as published in October 2026.
/// (https://platform.claude.com/docs/en/about-claude/pricing)
/// Gemini prices are the paid tier, <=200k-context where the price depends on it, from ai.google.dev/gemini-api/docs/pricing
/// (October 2026). Gemini 3.6-3.8 Flash are at their launch price, valid until the end of 2026.
/// OpenAI prices from developers.openai.com/api/docs/pricing (October 2026); a `-codex` variant costs like its base model
const TABLE: &[(&str, Price)] = &[
    // CLAUDE
    ("claude-fable-5-1", with_read(anthropic(10.0, 50.0), 0.25)),
    ("claude-opus-5-5", with_read(anthropic(4.0, 20.0), 0.20)),
    ("claude-sonnet-5-5", with_read(anthropic(2.0, 10.0), 0.10)),
    // TODO: decide how to manage the change of cost when prompts are up 100000 tokens, for now it's the highest, so just for prompts over
    // 100000 tokens
    ("claude-haiku-5-5", with_read(anthropic(0.50, 2.50), 0.05)),
    // only available to certain organizations
    ("claude-mythos-5-1", with_read(anthropic(10.0, 50.0), 0.25)),
    ("claude-fable-5", with_read(anthropic(10.0, 50.0), 1.0)),
    // only available to certain organizations
    ("claude-mythos-5", with_read(anthropic(10.0, 50.0), 1.0)),
    // i put them all, even if some of them are not available anymore (maybe some simple subagents will use them)
    ("claude-opus-5", with_read(anthropic(5.0, 25.0), 0.50)),
    ("claude-opus-4-8", with_read(anthropic(5.0, 25.0), 0.50)),
    ("claude-opus-4-7", with_read(anthropic(5.0, 25.0), 0.50)),
    ("claude-opus-4-6", with_read(anthropic(5.0, 25.0), 0.50)),
    ("claude-opus-4-5", with_read(anthropic(5.0, 25.0), 0.50)),
    ("claude-opus-4-1", with_read(anthropic(15.0, 75.0), 1.50)),
    ("claude-opus-4", with_read(anthropic(15.0, 75.0), 1.50)),
    ("claude-sonnet-5", with_read(anthropic(2.0, 5.0), 0.20)),
    ("claude-sonnet-4-6", with_read(anthropic(3.0, 15.0), 0.30)),
    ("claude-sonnet-4-5", with_read(anthropic(3.0, 15.0), 0.30)),
    ("claude-sonnet-4", with_read(anthropic(3.0, 15.0), 0.30)),
    ("claude-haiku-4-5", with_read(anthropic(1.0, 5.0), 0.10)),
    ("claude-haiku-3-5", with_read(anthropic(0.80, 4.0), 0.08)),
    // GEMINI
    ("gemini-3.8-flash", gemini(0.75, 3.75)),
    ("gemini-3.7-flash", gemini(0.75, 3.75)),
    ("gemini-3.6-flash", gemini(0.75, 3.75)),
    ("gemini-3.5-flash-lite", gemini(0.30, 2.50)),
    ("gemini-3.5-flash", gemini(1.50, 9.0)),
    ("gemini-3.1-flash-lite", gemini(0.25, 1.50)),
    ("gemini-3.1-pro", gemini(2.0, 12.0)),
    ("gemini-3-pro", gemini(2.0, 12.0)),
    ("gemini-3-flash", gemini(0.50, 3.0)),
    ("gemini-2.5-flash-lite", gemini(0.10, 0.40)),
    ("gemini-2.5-flash", gemini(0.30, 2.50)),
    ("gemini-2.5-pro", gemini(1.25, 10.0)),
    // CHATGPT
    ("gpt-6-astra", openai(10.0, 1.0, 50.0)),
    ("gpt-6.1-sol", openai(2.0, 0.01, 10.0)),
    ("gpt-6-luna", openai(0.1, 0.01, 0.5)),
    ("gpt-6-sol", openai(2.0, 0.2, 10.0)),
    ("gpt-5.6-sol", openai(4.0, 0.4, 20.0)),
    ("gpt-5.6-terra", openai(2.0, 0.2, 12.0)),
    ("gpt-5.6-luna", openai(0.2, 0.02, 0.02)),
    ("gpt-5.5", openai(5.0, 0.5, 30.0)),
    ("gpt-5.4-mini", openai(0.75, 0.075, 4.50)),
    ("gpt-5.4-nano", openai(0.20, 0.02, 1.25)),
    ("gpt-5.4", openai(2.5, 0.25, 15.0)),
    ("gpt-5.2", openai(1.75, 0.175, 14.0)),
    ("gpt-5.1-codex-mini", openai(0.25, 0.025, 2.0)),
    ("gpt-5.1", openai(1.25, 0.125, 10.0)),
    ("gpt-5.3-codex", openai(1.75, 0.175, 14.0)),
    ("gpt-5-mini", openai(0.25, 0.025, 2.0)),
    ("gpt-5-nano", openai(0.05, 0.005, 0.40)),
    ("gpt-5", openai(1.25, 0.125, 10.0)),
];

/// A prefix matches the whole id or the id up to a `-`: `gpt-5` matches `gpt-5-codex`, but not `gpt-5.9`, a newer model
/// with a price we do not know
pub fn price_for(model: &str) -> Option<Price> {
    TABLE
        .iter()
        .find(|(prefix, _)| {
            model
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('-'))
        })
        .map(|(_, p)| *p)
}

/// None, when the model is unknown
pub fn cost_usd(model: &str, u: &TokenUsage) -> Option<f64> {
    let p = price_for(model)?;
    let m = 1_000_000.0;
    Some(
        u.input as f64 * p.input / m
            + u.output as f64 * p.output / m
            + u.cache_write_5m as f64 * p.cache_write_5m / m
            + u.cache_write_1h as f64 * p.cache_write_1h / m
            + u.cache_read as f64 * p.cache_read / m,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn specific_prefix_wins() {
        assert_eq!(price_for("claude-opus-5-5").unwrap().input, 4.0);
        assert_eq!(price_for("claude-opus-5").unwrap().input, 5.0);
        assert_eq!(price_for("claude-opus-4-1-20250805").unwrap().input, 15.0);
        assert_eq!(price_for("claude-sonnet-4-5-20250929").unwrap().input, 3.0);
        assert_eq!(price_for("gemini-2.5-flash-lite").unwrap().input, 0.10);
        assert_eq!(price_for("gemini-3.5-flash-lite").unwrap().input, 0.30);
        assert_eq!(price_for("gemini-3.5-flash").unwrap().input, 1.50);
        assert_eq!(price_for("gemini-3.1-pro-preview").unwrap().output, 12.0);
        assert_eq!(price_for("gpt-5-codex").unwrap().input, 1.25);
        assert_eq!(price_for("gpt-5.1-codex-max").unwrap().input, 1.25);
        assert_eq!(price_for("gpt-5.2-codex").unwrap().input, 1.75);
        assert_eq!(price_for("gpt-5.4-mini").unwrap().cache_read, 0.075);
        assert_eq!(price_for("gpt-5.9"), None);
        assert_eq!(price_for("gpt-5.1-codex-mini").unwrap().input, 0.25);
        assert_eq!(price_for("gpt-5.4-nano").unwrap().input, 0.20);
    }

    #[test]
    fn cost_uses_every_counter() {
        let u = TokenUsage {
            input: 1_000_000,
            output: 1_000_000,
            cache_write_5m: 1_000_000,
            cache_write_1h: 1_000_000,
            cache_read: 1_000_000,
        };
        // opus 5.5: 4 + 20 + 5 + 8 + 0.20
        assert!(close(cost_usd("claude-opus-5-5", &u).unwrap(), 37.2));
        // fable 5.1: 10 + 50 + 12.5 +20 + 0.25
        assert!(close(cost_usd("claude-fable-5-1", &u).unwrap(), 92.75))
    }

    #[test]
    fn unknown_model_has_no_cost() {
        assert_eq!(cost_usd("<synthetic>", &TokenUsage::default()), None);
        assert_eq!(
            cost_usd(
                "gpt-9",
                &TokenUsage {
                    input: 5,
                    ..Default::default()
                }
            ),
            None
        );
    }
}
