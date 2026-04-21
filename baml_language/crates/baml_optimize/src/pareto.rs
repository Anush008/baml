//! Pareto frontier tracking for multi-objective candidate selection.
//!
//! The frontier maintains the set of *non-dominated* candidate indices given
//! a list of [`Objective`]s. A candidate `a` dominates `b` iff `a` is no
//! worse than `b` on every objective and strictly better on at least one.

use crate::candidate::{Candidate, CandidateScores};

/// Whether an objective is to be maximised or minimised.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Maximize,
    Minimize,
}

/// A single optimisation objective with a weight and extraction rule.
#[derive(Clone, Debug)]
pub struct Objective {
    pub name: String,
    pub direction: Direction,
    pub weight: f64,
}

impl Objective {
    pub fn new(name: &str, direction: Direction, weight: f64) -> Self {
        Self {
            name: name.to_string(),
            direction,
            weight,
        }
    }

    /// Pull this objective's scalar value out of a candidate's scores.
    ///
    /// Recognised names: `accuracy`, `tokens`, `latency`, and
    /// `accuracy:<testset>` for per-testset pass rates. Unknown names return
    /// `0.0`.
    pub fn extract(&self, scores: &CandidateScores) -> f64 {
        match self.name.as_str() {
            "accuracy" => scores.test_pass_rate,
            "tokens" => scores.avg_prompt_tokens + scores.avg_completion_tokens,
            "latency" => scores.avg_latency_ms,
            name if name.starts_with("accuracy:") => {
                let testset = &name["accuracy:".len()..];
                scores.per_testset_scores.get(testset).copied().unwrap_or(0.0)
            }
            _ => 0.0,
        }
    }
}

/// Tracks non-dominated candidate indices under a set of objectives.
pub struct ParetoFrontier {
    frontier: Vec<usize>,
    objectives: Vec<Objective>,
}

impl ParetoFrontier {
    pub fn new(objectives: Vec<Objective>) -> Self {
        Self {
            frontier: Vec::new(),
            objectives,
        }
    }

    /// `a` dominates `b` iff `a` is no worse on every objective and strictly
    /// better on at least one.
    fn dominates(&self, a: &CandidateScores, b: &CandidateScores) -> bool {
        let mut strictly_better_once = false;
        for obj in &self.objectives {
            let va = obj.extract(a);
            let vb = obj.extract(b);
            let (better, worse) = match obj.direction {
                Direction::Maximize => (va > vb, va < vb),
                Direction::Minimize => (va < vb, va > vb),
            };
            if worse {
                return false;
            }
            if better {
                strictly_better_once = true;
            }
        }
        strictly_better_once
    }

    /// Consider adding candidate `id` to the frontier. If it's dominated by a
    /// current frontier member, it's skipped. Frontier members that the new
    /// candidate dominates are removed.
    pub fn add(&mut self, id: usize, scores: &CandidateScores, candidates: &[Candidate]) {
        for &frontier_id in &self.frontier {
            if let Some(frontier_scores) = candidates[frontier_id].scores.as_ref() {
                if self.dominates(frontier_scores, scores) {
                    return;
                }
            }
        }

        // Collect first (can't mutate self.frontier while reading self.objectives via dominates()).
        let kept: Vec<usize> = self
            .frontier
            .iter()
            .copied()
            .filter(|&frontier_id| {
                candidates[frontier_id]
                    .scores
                    .as_ref()
                    .map_or(true, |fs| !self.dominates(scores, fs))
            })
            .collect();
        self.frontier = kept;
        self.frontier.push(id);
    }

    /// Current frontier indices (into a `Vec<Candidate>`).
    pub fn frontier(&self) -> &[usize] {
        &self.frontier
    }

    /// Pick the frontier member with the highest weighted score — used to
    /// choose the parent for the next reflection step.
    pub fn select_for_reflection(&self, candidates: &[Candidate]) -> Option<usize> {
        self.frontier.iter().copied().max_by(|&a, &b| {
            let sa = self.weighted_score(candidates[a].scores.as_ref());
            let sb = self.weighted_score(candidates[b].scores.as_ref());
            sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
        })
    }

    /// Pick the most diverse pair on the frontier — used as parents for a
    /// merge reflection. Returns `None` only when the frontier has fewer
    /// than two members; if every pair has identical scores (diversity 0),
    /// we still return the first pair so the caller can attempt a merge.
    pub fn select_for_merge(&self, candidates: &[Candidate]) -> Option<(usize, usize)> {
        if self.frontier.len() < 2 {
            return None;
        }
        // Seed with the first pair so a frontier of identical-score
        // candidates still yields Some(..).
        let mut best = (self.frontier[0], self.frontier[1]);
        let mut best_diversity = self.diversity(
            candidates[best.0].scores.as_ref(),
            candidates[best.1].scores.as_ref(),
        );
        for (i, &a) in self.frontier.iter().enumerate() {
            for &b in &self.frontier[i + 1..] {
                let d = self.diversity(
                    candidates[a].scores.as_ref(),
                    candidates[b].scores.as_ref(),
                );
                if d > best_diversity {
                    best_diversity = d;
                    best = (a, b);
                }
            }
        }
        Some(best)
    }

    fn weighted_score(&self, scores: Option<&CandidateScores>) -> f64 {
        let Some(scores) = scores else { return 0.0 };
        self.objectives
            .iter()
            .map(|obj| {
                let v = obj.extract(scores);
                let normalised = match obj.direction {
                    Direction::Maximize => v,
                    // Map minimise to a bounded "higher-is-better" range.
                    Direction::Minimize => 1.0 / (1.0 + v),
                };
                normalised * obj.weight
            })
            .sum()
    }

    fn diversity(&self, a: Option<&CandidateScores>, b: Option<&CandidateScores>) -> f64 {
        let (Some(a), Some(b)) = (a, b) else {
            return 0.0;
        };
        self.objectives
            .iter()
            .map(|obj| (obj.extract(a) - obj.extract(b)).abs())
            .sum()
    }
}

/// Parse objective-weight strings like `"accuracy=0.8,tokens=0.2"` or
/// `"accuracy:testset_a=0.5,latency=0.3"`.
///
/// `tokens` and `latency` default to `Minimize`; everything else defaults
/// to `Maximize`.
pub fn parse_objectives(weights_str: &str) -> Vec<Objective> {
    weights_str
        .split(',')
        .filter_map(|part| {
            let mut pieces = part.trim().splitn(2, '=');
            let name = pieces.next()?.trim();
            let weight: f64 = pieces.next()?.trim().parse().ok()?;
            if name.is_empty() {
                return None;
            }
            let direction = if name == "tokens" || name == "latency" {
                Direction::Minimize
            } else {
                Direction::Maximize
            };
            Some(Objective::new(name, direction, weight))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidate::{CandidateMethod, OptimizableFunction};
    use std::collections::HashMap;

    fn candidate_with_scores(id: usize, scores: CandidateScores) -> Candidate {
        Candidate {
            id,
            iteration: 0,
            parent_ids: vec![],
            method: CandidateMethod::Initial,
            function: OptimizableFunction {
                function_name: "F".into(),
                prompt_text: String::new(),
                classes: vec![],
                enums: vec![],
                function_source: None,
            },
            scores: Some(scores),
            rationale: None,
        }
    }

    fn scores(accuracy: f64, tokens: f64, latency: f64) -> CandidateScores {
        CandidateScores {
            test_pass_rate: accuracy,
            tests_passed: 0,
            tests_total: 0,
            avg_prompt_tokens: tokens,
            avg_completion_tokens: 0.0,
            avg_latency_ms: latency,
            per_testset_scores: HashMap::new(),
        }
    }

    #[test]
    fn parse_objectives_defaults_direction_by_name() {
        let objs = parse_objectives("accuracy=0.8,tokens=0.2");
        assert_eq!(objs.len(), 2);
        assert_eq!(objs[0].name, "accuracy");
        assert_eq!(objs[0].direction, Direction::Maximize);
        assert!((objs[0].weight - 0.8).abs() < 1e-9);
        assert_eq!(objs[1].name, "tokens");
        assert_eq!(objs[1].direction, Direction::Minimize);
    }

    #[test]
    fn parse_objectives_handles_per_testset_names() {
        let objs = parse_objectives("accuracy:set_a=0.5,latency=0.3");
        assert_eq!(objs[0].name, "accuracy:set_a");
        assert_eq!(objs[0].direction, Direction::Maximize);
        assert_eq!(objs[1].direction, Direction::Minimize);
    }

    #[test]
    fn parse_objectives_skips_malformed() {
        let objs = parse_objectives("accuracy=0.8,garbage,tokens=not_a_number,=0.5");
        assert_eq!(objs.len(), 1);
        assert_eq!(objs[0].name, "accuracy");
    }

    #[test]
    fn dominated_candidate_is_not_added() {
        let objectives = parse_objectives("accuracy=0.5,tokens=0.5");
        let mut frontier = ParetoFrontier::new(objectives);

        let a = candidate_with_scores(0, scores(0.9, 100.0, 0.0));
        let b = candidate_with_scores(1, scores(0.5, 200.0, 0.0));
        let candidates = vec![a, b];

        frontier.add(0, candidates[0].scores.as_ref().unwrap(), &candidates);
        frontier.add(1, candidates[1].scores.as_ref().unwrap(), &candidates);

        // b is strictly worse on both axes (lower accuracy, higher tokens)
        assert_eq!(frontier.frontier(), &[0]);
    }

    #[test]
    fn dominating_candidate_evicts_dominated_members() {
        let objectives = parse_objectives("accuracy=0.5,tokens=0.5");
        let mut frontier = ParetoFrontier::new(objectives);

        let weak = candidate_with_scores(0, scores(0.5, 200.0, 0.0));
        let strong = candidate_with_scores(1, scores(0.9, 100.0, 0.0));
        let candidates = vec![weak, strong];

        frontier.add(0, candidates[0].scores.as_ref().unwrap(), &candidates);
        assert_eq!(frontier.frontier(), &[0]);

        frontier.add(1, candidates[1].scores.as_ref().unwrap(), &candidates);
        assert_eq!(frontier.frontier(), &[1], "strong evicts weak");
    }

    #[test]
    fn non_comparable_candidates_both_stay() {
        let objectives = parse_objectives("accuracy=0.5,tokens=0.5");
        let mut frontier = ParetoFrontier::new(objectives);

        // Each candidate wins on one axis, loses on the other — neither dominates.
        let a = candidate_with_scores(0, scores(0.9, 200.0, 0.0));
        let b = candidate_with_scores(1, scores(0.7, 100.0, 0.0));
        let candidates = vec![a, b];

        frontier.add(0, candidates[0].scores.as_ref().unwrap(), &candidates);
        frontier.add(1, candidates[1].scores.as_ref().unwrap(), &candidates);

        let mut f = frontier.frontier().to_vec();
        f.sort_unstable();
        assert_eq!(f, vec![0, 1]);
    }

    #[test]
    fn select_for_reflection_picks_best_weighted() {
        let objectives = parse_objectives("accuracy=1.0");
        let mut frontier = ParetoFrontier::new(objectives);

        let lo = candidate_with_scores(0, scores(0.5, 0.0, 0.0));
        let hi = candidate_with_scores(1, scores(0.9, 0.0, 0.0));
        let candidates = vec![lo, hi];

        frontier.add(0, candidates[0].scores.as_ref().unwrap(), &candidates);
        frontier.add(1, candidates[1].scores.as_ref().unwrap(), &candidates);

        assert_eq!(frontier.select_for_reflection(&candidates), Some(1));
    }

    #[test]
    fn select_for_merge_needs_two_members() {
        let objectives = parse_objectives("accuracy=1.0");
        let mut frontier = ParetoFrontier::new(objectives);

        let only = candidate_with_scores(0, scores(0.5, 0.0, 0.0));
        let candidates = vec![only];

        frontier.add(0, candidates[0].scores.as_ref().unwrap(), &candidates);
        assert_eq!(frontier.select_for_merge(&candidates), None);
    }
}
