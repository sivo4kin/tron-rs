//! Market price integer math — a faithful port of java-tron
//! `MarketComparator.comparePrice` / `MarketUtils.priceMatch` /
//! `MarketUtils.multiplyAndDivide`.
//!
//! A price is a `(sell_quantity, buy_quantity)` pair (rational sell/buy). All
//! comparisons use `i128` to match java's `long`→`BigInteger` overflow fallback.

use std::cmp::Ordering;

/// java-tron `MarketComparator.comparePrice(p1Sell, p1Buy, p2Sell, p2Buy)` =
/// `sign(p1Buy*p2Sell - p2Buy*p1Sell)`.
pub fn compare_price(p1_sell: i64, p1_buy: i64, p2_sell: i64, p2_buy: i64) -> Ordering {
    (p1_buy as i128 * p2_sell as i128).cmp(&(p2_buy as i128 * p1_sell as i128))
}

/// Order two maker prices for the book: **lowest price first** (best for a
/// taker). Mirrors the ascending `comparePriceKey` ordering.
pub fn cmp_maker_price(a: (i64, i64), b: (i64, i64)) -> Ordering {
    compare_price(a.0, a.1, b.0, b.1)
}

/// java-tron `MarketUtils.priceMatch`: does the taker's price cross the maker's?
/// `comparePrice(takerBuy, takerSell, makerSell, makerBuy) >= 0`, i.e.
/// `takerSell*makerSell >= takerBuy*makerBuy`.
pub fn price_match(taker_sell: i64, taker_buy: i64, maker_sell: i64, maker_buy: i64) -> bool {
    compare_price(taker_buy, taker_sell, maker_sell, maker_buy) != Ordering::Less
}

/// java-tron `MarketUtils.multiplyAndDivide` = `floor(a*b/c)` (positive operands).
pub fn multiply_and_divide(a: i64, b: i64, c: i64) -> i64 {
    ((a as i128 * b as i128) / c as i128) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compare_price_sign() {
        // price 1/2 vs 1/3: 1/2 > 1/3 (more buy per sell) => Greater.
        assert_eq!(compare_price(2, 1, 3, 1), Ordering::Greater);
        assert_eq!(compare_price(3, 1, 2, 1), Ordering::Less);
        assert_eq!(compare_price(2, 1, 4, 2), Ordering::Equal);
    }

    #[test]
    fn price_match_crossing() {
        // taker sells 100 TRX wanting 10 A (price 10 TRX/A). maker sells 10 A
        // wanting 50 TRX (price 5 TRX/A): taker pays up to 10, maker asks 5 -> match.
        assert!(price_match(100, 10, 10, 50));
        // maker asks 200 TRX for 10 A (20 TRX/A) > taker's 10 -> no match.
        assert!(!price_match(100, 10, 10, 200));
    }

    #[test]
    fn multiply_and_divide_floors() {
        assert_eq!(multiply_and_divide(100, 3, 7), 42); // 300/7 = 42.85 -> 42
        assert_eq!(multiply_and_divide(i64::MAX, 2, 4), i64::MAX / 2); // no overflow (i128)
    }
}
