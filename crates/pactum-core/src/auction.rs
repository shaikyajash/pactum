//! Dutch-auction pricing and secret-bucket selection, mirroring `PactumIntents`.

use alloy::primitives::U256;
use thiserror::Error;

use crate::Order;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FillError {
    #[error("single-part orders must be filled whole")]
    PartialFillNotAllowed,
}

/// Destination amount asked for the whole order at time `t`.
pub fn price(o: &Order, t: u64) -> U256 {
    if t <= o.auctionStart {
        return o.startDstAmount;
    }
    if t >= o.auctionEnd {
        return o.endDstAmount;
    }
    let elapsed = U256::from(t - o.auctionStart);
    let span = U256::from(o.auctionEnd - o.auctionStart);
    o.startDstAmount - (o.startDstAmount - o.endDstAmount) * elapsed / span
}

/// Destination amount a fill of `fill` owes at time `t`, rounded up for the maker.
pub fn owed(o: &Order, fill: U256, t: u64) -> U256 {
    (price(o, t) * fill + o.srcAmount - U256::from(1)) / o.srcAmount
}

/// Secret index a fill must use given the cumulative filled amount after it.
pub fn secret_index(src_amount: U256, parts: u16, filled_after: U256) -> Result<usize, FillError> {
    if parts == 1 {
        return if filled_after == src_amount {
            Ok(0)
        } else {
            Err(FillError::PartialFillNotAllowed)
        };
    }
    if filled_after == src_amount {
        return Ok(parts as usize);
    }
    Ok(((filled_after * U256::from(parts) - U256::from(1)) / src_amount).to::<usize>())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order() -> Order {
        Order {
            srcAmount: U256::from(100),
            startDstAmount: U256::from(5000),
            endDstAmount: U256::from(3000),
            auctionStart: 1000,
            auctionEnd: 1600,
            parts: 4,
            ..Default::default()
        }
    }

    #[test]
    fn decays_linearly_and_clamps() {
        let o = order();
        assert_eq!(price(&o, 0), U256::from(5000));
        assert_eq!(price(&o, 1300), U256::from(4000));
        assert_eq!(price(&o, 9999), U256::from(3000));
    }

    #[test]
    fn owed_is_pro_rata_rounded_up() {
        let o = order();
        assert_eq!(owed(&o, U256::from(50), 1300), U256::from(2000));
        let flat = Order { startDstAmount: U256::from(1), endDstAmount: U256::from(1), ..o };
        assert_eq!(owed(&flat, U256::from(1), 0), U256::from(1));
    }

    #[test]
    fn buckets_match_contract() {
        let cases = [(1, 0), (25, 0), (26, 1), (50, 1), (75, 2), (99, 3), (100, 4)];
        for (filled, idx) in cases {
            assert_eq!(secret_index(U256::from(100), 4, U256::from(filled)), Ok(idx), "{filled}");
        }
        assert_eq!(
            secret_index(U256::from(100), 1, U256::from(50)),
            Err(FillError::PartialFillNotAllowed)
        );
    }
}
