//! Conversions between this workspace's domain types (`oms_core`,
//! `tick_math`) and the generated proto types. All FixedX18/OrderId values
//! cross the wire as decimal strings of their raw integer, see
//! `execution.proto`'s module doc for why.

use tick_math::FixedX18;

use crate::proto;
use crate::BridgeError;

pub fn side_to_proto(side: oms_core::Side) -> proto::Side {
    match side {
        oms_core::Side::Long => proto::Side::Long,
        oms_core::Side::Short => proto::Side::Short,
    }
}

pub fn side_from_proto(side: proto::Side) -> Result<oms_core::Side, BridgeError> {
    match side {
        proto::Side::Long => Ok(oms_core::Side::Long),
        proto::Side::Short => Ok(oms_core::Side::Short),
        proto::Side::Unspecified => Err(BridgeError::InvalidResponse("unspecified Side".into())),
    }
}

pub fn tif_to_proto(tif: oms_core::TimeInForce) -> proto::TimeInForce {
    match tif {
        oms_core::TimeInForce::Gtc => proto::TimeInForce::Gtc,
        oms_core::TimeInForce::Ioc => proto::TimeInForce::Ioc,
        oms_core::TimeInForce::Fok => proto::TimeInForce::Fok,
        oms_core::TimeInForce::Alo => proto::TimeInForce::Alo,
        oms_core::TimeInForce::SoftAlo => proto::TimeInForce::SoftAlo,
    }
}

pub fn fixed_to_string(value: FixedX18) -> String {
    value.inner().to_string()
}

pub fn fixed_from_string(s: &str) -> Result<FixedX18, BridgeError> {
    s.parse::<i128>()
        .map(FixedX18::raw)
        .map_err(|e| BridgeError::InvalidResponse(format!("bad FixedX18 '{s}': {e}")))
}

pub fn order_id_to_string(id: oms_core::OrderId) -> String {
    id.raw().to_string()
}

pub fn order_id_from_string(s: &str) -> Result<oms_core::OrderId, BridgeError> {
    let raw: u64 = s.parse().map_err(|e| BridgeError::InvalidResponse(format!("bad OrderId '{s}': {e}")))?;
    // OrderId has no public raw-u64 constructor (by design, from_parts is
    // the only way to build one from semantic fields). Round-trip through
    // unpack/from_parts to validate the initialized-marker bit and layout
    // instead of transmuting an arbitrary u64.
    let side = if (raw >> 56) & 1 == 1 { oms_core::Side::Short } else { oms_core::Side::Long };
    if raw & (1u64 << 63) == 0 {
        return Err(BridgeError::InvalidResponse(format!("OrderId '{s}' missing initialized-marker bit")));
    }
    let order_index = raw & ((1u64 << 40) - 1);
    let encoded_tick = ((raw >> 40) & 0xFFFF) as u16;
    // re-derive tick_index using the same transform oms_core::OrderId uses
    // internally (duplicated here since it's private to that crate, see
    // TODO below)
    let e = if matches!(side, oms_core::Side::Long) { !encoded_tick } else { encoded_tick };
    let tick_index = (e ^ (1u16 << 15)) as i16;

    // TODO: oms_core::OrderId doesn't expose a `from_raw`/`try_from(u64)`
    // constructor today, only `from_parts(side, tick, order_index)`, which
    // re-encodes instead of trusting the wire value directly. That's
    // safer (never trust a wire-supplied bit pattern without validating it
    // decodes to something the encoder would produce), but it
    // does mean this function duplicates oms_core's private tick decode
    // logic instead of calling it. If that logic ever changes, this must
    // change with it. Consider exposing `OrderId::from_raw_checked(u64)`
    // in oms_core instead, once execution-adapter's needs are stable.
    oms_core::OrderId::from_parts(side, tick_index, order_index).map_err(|e| BridgeError::InvalidResponse(format!("OrderId '{s}' round-trip failed: {e}")))
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn side_roundtrips() {
        assert_eq!(side_from_proto(side_to_proto(oms_core::Side::Long)).unwrap(), oms_core::Side::Long);
        assert_eq!(side_from_proto(side_to_proto(oms_core::Side::Short)).unwrap(), oms_core::Side::Short);
    }

    #[test]
    fn unspecified_side_is_rejected() {
        assert!(side_from_proto(proto::Side::Unspecified).is_err());
    }

    #[test]
    fn fixed_roundtrips_including_negative() {
        let v = FixedX18::from_f64(-12.34);
        let s = fixed_to_string(v);
        assert_eq!(fixed_from_string(&s).unwrap(), v);
    }

    #[test]
    fn order_id_roundtrips_through_string() {
        let id = oms_core::OrderId::from_parts(oms_core::Side::Short, -500, 42).unwrap();
        let s = order_id_to_string(id);
        let back = order_id_from_string(&s).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn order_id_without_marker_bit_rejected() {
        let err = order_id_from_string("12345").unwrap_err();
        assert!(matches!(err, BridgeError::InvalidResponse(_)));
    }
}
