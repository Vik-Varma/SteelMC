//! Integer sequences shared by vanilla search loops.

use std::iter;

/// Yields values moving outward from `origin` (clamped into the bounds), alternating
/// above and below it in `step_size` increments and continuing on one side once the
/// other runs out of room. Both bounds are inclusive.
///
/// # Panics
///
/// Panics if `lower_bound > upper_bound` or `step_size < 1`.
pub fn out_from_origin(
    origin: i32,
    lower_bound: i32,
    upper_bound: i32,
    step_size: i32,
) -> impl Iterator<Item = i32> {
    assert!(
        lower_bound <= upper_bound,
        "upper bound {upper_bound} must not be below lower bound {lower_bound}"
    );
    assert!(
        step_size >= 1,
        "step size must be at least 1, was {step_size}"
    );

    let origin = origin.clamp(lower_bound, upper_bound);
    iter::successors(Some(origin), move |&cursor| {
        let distance = (origin - cursor).abs();
        let previous_was_negative = cursor <= origin;
        let can_move_positive = origin + distance + step_size <= upper_bound;
        let negative = origin - distance - if previous_was_negative { step_size } else { 0 };
        let take_negative =
            (!previous_was_negative || !can_move_positive) && negative >= lower_bound;
        Some(if take_negative {
            negative
        } else {
            origin + distance + step_size
        })
    })
    .take_while(move |&cursor| {
        let distance = (origin - cursor).abs();
        origin - distance >= lower_bound || origin + distance <= upper_bound
    })
}

#[cfg(test)]
mod tests {
    use super::out_from_origin;

    fn collect(origin: i32, lower: i32, upper: i32, step: i32) -> Vec<i32> {
        out_from_origin(origin, lower, upper, step).collect()
    }

    #[test]
    fn alternates_above_then_below_the_origin() {
        assert_eq!(collect(5, 0, 10, 1), [5, 6, 4, 7, 3, 8, 2, 9, 1, 10, 0]);
    }

    #[test]
    fn continues_on_one_side_after_the_other_runs_out() {
        // `locate biome` heights for an overworld origin at y=64.
        assert_eq!(collect(64, -63, 320, 64), [64, 128, 0, 192, 256, 320]);
        assert_eq!(collect(2, 0, 9, 4), [2, 6]);
    }

    #[test]
    fn clamps_an_origin_outside_the_bounds() {
        assert_eq!(collect(-100, -63, 320, 64), [-63, 1, 65, 129, 193, 257]);
        assert_eq!(collect(400, -63, 320, 64), [320, 256, 192, 128, 64, 0]);
    }
}
