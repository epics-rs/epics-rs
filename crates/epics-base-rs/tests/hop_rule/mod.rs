//! A chain's targets recurse while the thread has the stack for them and run
//! as hop frames that park below that (`hop_floor`), in the same order.

#![allow(dead_code)]

use std::future::Future;

use epics_base_rs::server::database::hop_floor::{self, Rule};

/// `case` with every target recursing, with every target a hop frame, and
/// with the switch from one to the other just above and just below each
/// address the recursing run entered a target at. `case` builds its own
/// records and enters its chain from one frame, so every run enters at the
/// recursing run's addresses up to its switch; it is handed the rule's name.
pub async fn sweep<F, Fut>(mut case: F)
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = ()>,
{
    hop_floor::set(Rule::Log);
    case("every target recursing".into()).await;
    let entered = hop_floor::take_log();
    hop_floor::set(Rule::Log);
    case("every target recursing, again".into()).await;
    assert_eq!(
        hop_floor::take_log(),
        entered,
        "a run enters its targets at the addresses the last one did"
    );
    assert!(
        !entered.is_empty(),
        "the case enters a target on this thread"
    );
    let mut floors = entered.clone();
    floors.sort_unstable();
    floors.dedup();
    for at in floors {
        let i = entered.iter().position(|&a| a == at).unwrap();
        hop_floor::set(Rule::Floor(at));
        case(format!("target {i} the first hop frame")).await;
        hop_floor::set(Rule::Floor(at - 1));
        case(format!("target {i} the last to recurse")).await;
    }
    hop_floor::set(Rule::Park);
    case("every target a hop frame".into()).await;
    hop_floor::set(Rule::Os);
}
