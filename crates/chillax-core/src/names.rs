//! Default names for teammates hired without one.

use crate::engine::State;

const NAMES: &[&str] = &[
    "Ada",
    "Grace",
    "Linus",
    "Alan",
    "Margaret",
    "Dennis",
    "Ken",
    "Barbara",
    "Guido",
    "Bjarne",
    "Radia",
    "Tim",
    "Vint",
    "Hedy",
    "Donald",
    "Edsger",
    "Frances",
    "Katherine",
    "Anders",
    "Brendan",
    "James",
    "Yukihiro",
    "Rob",
    "Larry",
    "Jean",
    "Sophie",
    "Fei-Fei",
    "Andrew",
    "Geoffrey",
    "Yann",
    "Satoshi",
    "Steve",
    "Woz",
    "Bill",
    "Richard",
    "Brian",
    "Leslie",
    "Niklaus",
    "John",
    "Claude",
    "Marissa",
    "Sheryl",
    "Jeff",
    "Demis",
    "Ilya",
    "Evan",
    "Dries",
    "Ryan",
    "Kelsey",
    "Lisa",
    "Sundar",
    "Satya",
    "Jensen",
    "Vitalik",
    "Nadia",
    "Joan",
    "Mitchell",
    "Whitfield",
    "Shafi",
    "Judea",
    "Cynthia",
];

/// The first name in the list no employee has, so names stay unique and replay the same.
/// Falls back to `"{fallback} {n}"` when the list runs out.
pub fn default_name(state: &State, fallback: &str) -> String {
    let taken = |name: &str| {
        state
            .employees
            .iter()
            .any(|e| e.employee.name.eq_ignore_ascii_case(name))
    };
    if let Some(name) = NAMES.iter().find(|n| !taken(n)) {
        return (*name).to_string();
    }
    (2..)
        .map(|n| format!("{fallback} {n}"))
        .find(|name| !taken(name))
        .expect("an unbounded range always yields a free name")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_free_name_is_picked() {
        let state = State::default();
        assert_eq!(default_name(&state, "Dev"), "Ada");
    }
}
