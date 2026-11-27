//! What a source's answers cost: prices in integer micro-USD per million
//! tokens, a machine's draw in watts, and each usage row's cost in integer
//! nano-USD, worked out once when the row is written. Never a float.

use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// The highest price per million tokens, in micro-USD.
const PRICE_MAX: i64 = 10_000_000_000;
/// The highest electricity price per kWh, in micro-USD.
pub const KWH_MAX: i64 = 100_000_000;
/// `electricityPerKwh` until an admin sets it: 0.15 USD.
pub const KWH_DEFAULT: i64 = 150_000;
const WATTS_MAX: u32 = 100_000;
const MODELS_MAX: usize = 500;
/// The longest model id an override may name.
const MODEL_ID_MAX: usize = 200;

/// A token price pair; `None` falls back to the source's own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Price {
    pub prompt: Option<i64>,
    pub completion: Option<i64>,
}

/// A source's pricing, with each model's override.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pricing {
    pub price: Price,
    pub watts: Option<i64>,
    pub models: BTreeMap<String, Price>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct PriceIn {
    prompt_per_million: Option<String>,
    completion_per_million: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct PricingIn {
    prompt_per_million: Option<String>,
    completion_per_million: Option<String>,
    watts: Option<u32>,
    models: Option<BTreeMap<String, PriceIn>>,
}

/// A decimal string of at most 6 places as integer micro-units, `None`
/// when it isn't one or is over `max`.
pub fn micros(s: &str, max: i64) -> Option<i64> {
    let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
    let digits = |t: &str| t.bytes().all(|b| b.is_ascii_digit());
    if whole.is_empty() || !digits(whole) || !digits(frac) || frac.len() > 6 {
        return None;
    }
    if s.contains('.') && frac.is_empty() {
        return None;
    }
    let mut n: i64 = 0;
    for b in whole.bytes() {
        n = n.checked_mul(10)?.checked_add(i64::from(b - b'0'))?;
    }
    let frac: i64 = format!("{frac:0<6}").parse().ok()?;
    let n = n.checked_mul(1_000_000)?.checked_add(frac)?;
    (n <= max).then_some(n)
}

/// Integer micro-units as the shortest decimal string.
pub fn decimal(m: i64) -> String {
    let (whole, frac) = (m / 1_000_000, m % 1_000_000);
    if frac == 0 {
        return whole.to_string();
    }
    let frac = format!("{frac:06}");
    format!("{whole}.{}", frac.trim_end_matches('0'))
}

fn price_in(p: Option<&str>) -> Result<Option<i64>, ()> {
    p.map(|s| micros(s, PRICE_MAX).ok_or(())).transpose()
}

impl Price {
    fn json(&self) -> Value {
        json!({
            "promptPerMillion": self.prompt.map(decimal),
            "completionPerMillion": self.completion.map(decimal),
        })
    }
}

impl Pricing {
    /// The `pricing` of a request or of a stored row: `Some(None)` clears
    /// it, as does one with nothing set, and `None` is a bad shape.
    pub fn parse(v: &Value) -> Option<Option<Pricing>> {
        let p: Option<PricingIn> = serde_json::from_value(v.clone()).ok()?;
        let Some(p) = p else {
            return Some(None);
        };
        let watts = p.watts.filter(|w| (1..=WATTS_MAX).contains(w));
        if p.watts.is_some() && watts.is_none() {
            return None;
        }
        let models = p.models.unwrap_or_default();
        if models.len() > MODELS_MAX {
            return None;
        }
        let mut out = Pricing {
            price: Price {
                prompt: price_in(p.prompt_per_million.as_deref()).ok()?,
                completion: price_in(p.completion_per_million.as_deref()).ok()?,
            },
            watts: watts.map(i64::from),
            models: BTreeMap::new(),
        };
        for (id, m) in models {
            if id.is_empty() || id.len() > MODEL_ID_MAX || id.chars().any(char::is_control) {
                return None;
            }
            let price = Price {
                prompt: price_in(m.prompt_per_million.as_deref()).ok()?,
                completion: price_in(m.completion_per_million.as_deref()).ok()?,
            };
            out.models.insert(id, price);
        }
        Some((out != Pricing::default()).then_some(out))
    }

    pub fn json(&self) -> Value {
        let mut v = self.price.json();
        v["watts"] = json!(self.watts);
        let models: serde_json::Map<String, Value> = self
            .models
            .iter()
            .map(|(id, p)| (id.clone(), p.json()))
            .collect();
        v["models"] = Value::Object(models);
        v
    }
}

/// The fields that differ between two pricings, as `PATCH` names them.
pub fn changed(was: Option<&Pricing>, now: Option<&Pricing>) -> Vec<&'static str> {
    let none = Pricing::default();
    let (a, b) = (was.unwrap_or(&none), now.unwrap_or(&none));
    let mut out = vec![];
    if a.price.prompt != b.price.prompt {
        out.push("pricing.promptPerMillion");
    }
    if a.price.completion != b.price.completion {
        out.push("pricing.completionPerMillion");
    }
    if a.watts != b.watts {
        out.push("pricing.watts");
    }
    if a.models != b.models {
        out.push("pricing.models");
    }
    out
}

/// One usage row's cost in nano-USD and whether the source had pricing.
/// `model` is the upstream id, or `None` for a row naming only the source.
pub fn cost(
    p: Option<&Pricing>,
    model: Option<&str>,
    prompt: i64,
    completion: i64,
    ms: i64,
    kwh: i64,
) -> (i64, bool) {
    let Some(p) = p else {
        return (0, false);
    };
    let over = model.and_then(|m| p.models.get(m));
    let pick = |f: fn(&Price) -> Option<i64>| {
        over.and_then(f)
            .or_else(|| f(&p.price))
            .map_or(0, i128::from)
    };
    let tokens = i128::from(prompt.max(0)) * pick(|x| x.prompt) / 1000
        + i128::from(completion.max(0)) * pick(|x| x.completion) / 1000;
    let power = p.watts.map_or(0, |w| {
        i128::from(w) * i128::from(ms.max(0)) * i128::from(kwh) / 3_600_000
    });
    (i64::try_from(tokens + power).unwrap_or(i64::MAX), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_15_extreme_inputs_cost_exactly() {
        let p = Pricing::parse(&json!({
            "promptPerMillion": "10000",
            "completionPerMillion": "10000",
            "watts": 100000,
        }))
        .unwrap();
        let day = 86_400_000;
        let tokens = 1_000_000_000;
        let (n, known) = cost(p.as_ref(), None, tokens, tokens, day, KWH_MAX);
        assert_eq!(n, 20_000_000_000_000_000 + 240_000_000_000_000);
        assert!(known);
        let (n, _) = cost(p.as_ref(), None, i64::MAX, i64::MAX, i64::MAX, KWH_MAX);
        assert_eq!(n, i64::MAX);
    }

    #[test]
    fn prices_are_read_and_written_as_decimal_strings() {
        assert_eq!(micros("0.15", PRICE_MAX), Some(150_000));
        assert_eq!(micros("3", PRICE_MAX), Some(3_000_000));
        assert_eq!(micros("0.000001", PRICE_MAX), Some(1));
        assert_eq!(micros("10000", PRICE_MAX), Some(PRICE_MAX));
        for bad in [
            "10000.000001",
            "1e3",
            "-1",
            "",
            ".5",
            "1.",
            "0.0000001",
            " 1",
        ] {
            assert_eq!(micros(bad, PRICE_MAX), None, "{bad:?}");
        }
        for (m, s) in [
            (150_000, "0.15"),
            (3_000_000, "3"),
            (1, "0.000001"),
            (0, "0"),
        ] {
            assert_eq!(decimal(m), s);
        }
    }
}
