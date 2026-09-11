//! Currency handling for the status line's two money readouts.
//!
//! The session cost and the account balance come from unrelated sources and can
//! be denominated differently. `price_in`/`price_out` are the user's price list
//! and are quoted in `price_currency` (USD by default, matching the built-in
//! DeepSeek numbers), while the balance is whatever the provider says the
//! account is billed in — DeepSeek answers CNY for China-region accounts
//! (`balance_infos: [{currency: CNY, …}, {currency: USD, total_balance: 0}]`)
//! and USD elsewhere. Printing `$0.03` next to `¥135.70` invites a comparison
//! that isn't valid, so both figures carry their own currency and the ISO code
//! is appended whenever the two differ.

/// An ISO 4217 currency and the symbol used to print it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Currency {
    /// Uppercase ISO code, e.g. `USD`.
    pub code: String,
    /// Display symbol, e.g. `$`. Empty when we have none, in which case the
    /// code itself is used as the prefix so the amount is never unitless.
    pub symbol: String,
}

/// Known symbols, keyed by ISO code. Not exhaustive: `Currency::parse` falls
/// back to printing the code, which is still unambiguous.
const SYMBOLS: &[(&str, &str)] = &[
    ("USD", "$"),
    ("CNY", "¥"),
    ("JPY", "¥"),
    ("EUR", "€"),
    ("GBP", "£"),
    ("INR", "₹"),
    ("KRW", "₩"),
    ("RUB", "₽"),
    ("BRL", "R$"),
    ("CAD", "CA$"),
    ("AUD", "A$"),
    ("HKD", "HK$"),
    ("TWD", "NT$"),
    ("SGD", "S$"),
    ("NZD", "NZ$"),
    ("CHF", "CHF"),
];

impl Currency {
    /// Build from an ISO code, case-insensitively. An empty or unknown code
    /// keeps the code as the prefix (so `$` is never assumed) — except that a
    /// blank code degrades to USD, the price list's documented unit.
    pub fn parse(code: &str) -> Currency {
        let code = code.trim().to_ascii_uppercase();
        if code.is_empty() {
            return Currency::parse("USD");
        }
        let symbol = SYMBOLS
            .iter()
            .find(|(c, _)| *c == code)
            .map(|(_, s)| s.to_string())
            .unwrap_or_default();
        Currency { code, symbol }
    }

    pub fn usd() -> Currency {
        Currency::parse("USD")
    }

    /// `$0.0123`. `decimals` controls how many places are shown.
    pub fn amount(&self, value: f64, decimals: usize) -> String {
        let n = format!("{value:.decimals$}");
        if self.symbol.is_empty() {
            format!("{} {n}", self.code)
        } else {
            format!("{}{n}", self.symbol)
        }
    }

    /// `$0.0123 USD` — the amount plus the ISO code, for lines that show more
    /// than one currency (or where the symbol is ambiguous, like `¥` for both
    /// CNY and JPY).
    pub fn amount_tagged(&self, value: f64, decimals: usize) -> String {
        format!("{} {}", self.amount(value, decimals), self.code)
    }
}

/// An account balance exactly as the provider reports it: the provider's own
/// decimal string (kept so `135.70` doesn't become `135.7`) plus the currency
/// the account is billed in.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Balance {
    pub amount: String,
    pub currency: Currency,
}

impl Balance {
    /// `¥135.70`.
    pub fn render(&self) -> String {
        self.amount_with(&self.currency)
    }

    /// `¥135.70 CNY`.
    pub fn render_tagged(&self) -> String {
        format!("{} {}", self.render(), self.currency.code)
    }

    fn amount_with(&self, cur: &Currency) -> String {
        if cur.symbol.is_empty() {
            format!("{} {}", cur.code, self.amount)
        } else {
            format!("{}{}", cur.symbol, self.amount)
        }
    }
}

/// Session cost in `cur`: sub-cent amounts get four decimals so a short turn
/// isn't reported as `$0.00`, everything else two.
pub fn fmt_cost(cost: f64, cur: &Currency) -> String {
    let decimals = if cost.abs() < 0.01 { 4 } else { 2 };
    cur.amount(cost, decimals)
}

/// [`fmt_cost`] with the ISO code appended, for status lines that also show a
/// balance in a different currency.
pub fn fmt_cost_tagged(cost: f64, cur: &Currency) -> String {
    let decimals = if cost.abs() < 0.01 { 4 } else { 2 };
    cur.amount_tagged(cost, decimals)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_is_case_insensitive_and_knows_the_common_symbols() {
        assert_eq!(Currency::parse("usd").code, "USD");
        assert_eq!(Currency::parse(" USD ").symbol, "$");
        assert_eq!(Currency::parse("cny").symbol, "¥");
        assert_eq!(Currency::parse("eur").symbol, "€");
        assert_eq!(Currency::parse("").code, "USD", "blank falls back to USD");
    }

    #[test]
    fn an_unknown_code_prints_the_code_instead_of_a_wrong_symbol() {
        let c = Currency::parse("xau");
        assert_eq!(c.code, "XAU");
        assert!(c.symbol.is_empty());
        assert_eq!(c.amount(12.5, 2), "XAU 12.50");
        assert_eq!(c.amount_tagged(12.5, 2), "XAU 12.50 XAU");
    }

    #[test]
    fn cost_keeps_four_decimals_below_a_cent() {
        let usd = Currency::usd();
        assert_eq!(fmt_cost(0.0, &usd), "$0.0000");
        assert_eq!(fmt_cost(0.0034, &usd), "$0.0034");
        assert_eq!(fmt_cost(0.01, &usd), "$0.01");
        assert_eq!(fmt_cost(1.239, &usd), "$1.24");
        assert_eq!(fmt_cost(0.0034, &Currency::parse("CNY")), "¥0.0034");
        assert_eq!(fmt_cost_tagged(0.0034, &usd), "$0.0034 USD");
    }

    #[test]
    fn balance_keeps_the_providers_own_digits() {
        let cny = Balance {
            amount: "135.70".into(),
            currency: Currency::parse("CNY"),
        };
        assert_eq!(cny.render(), "¥135.70");
        assert_eq!(cny.render_tagged(), "¥135.70 CNY");

        let od = Balance {
            amount: "0.00".into(),
            currency: Currency::usd(),
        };
        assert_eq!(od.render(), "$0.00");
    }
}
