use crate::model::{
    CorrNodeLaneUpdates, CorrResponse, CorrReviewerLaneEvidence, CorrReviewerPhaseEvidence,
    CorrStatus, CorrTargetLaneUpdates, DeviationId, DeviationLaneUpdates, LaneId, NodeId,
    PaperResponse, PaperReviewerLaneEvidence, ResponseStatus, SoundLaneUpdates, SoundResponse,
    SoundReviewerDecisionEvidence, SoundReviewerLaneEvidence, SoundStatus,
    SubstantivenessLaneUpdates, SubstantivenessStatus, TargetId, Update, VerifierIssue,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const PREAMBLE_NAME: &str = "Preamble";

/// Per-node correspondence verdict carried by `RawCorrPhasePayload` when the
/// payload describes the *correspondence* lane (Lean-vs-TeX node alignment).
/// Each verdict carries an explicit `verdict` (Pass / Fail) and an optional
/// `comment` (required when `verdict == Fail`, validated by
/// `validate_correspondence_result_data`).
///
/// The paper-faithfulness lane shares `RawCorrPhasePayload` but uses the
/// legacy `issues[]` field; verdicts stay empty for paper-faithfulness
/// emissions and the corr-node lane consumes them directly.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RawCorrVerdict {
    pub node: String,
    pub verdict: String,
    pub comment: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RawCorrPhasePayload {
    pub decision: String,
    /// Used only by the *paper-faithfulness* lane (target-level coverage
    /// findings). The corr-node lane leaves this empty.
    pub issues: Vec<VerifierIssue>,
    /// Used only by the *corr-node* lane. Each entry is an explicit per-node
    /// vote — silence on a node defaults to `CorrStatus::Fail` in
    /// `normalize_corr_response`, which forces the verifier to actually
    /// examine every node it was given (mirrors the substantiveness lane's
    /// post-2026-04 behavior). Paper-faithfulness leaves this empty.
    pub verdicts: Vec<RawCorrVerdict>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RawCorrPayload {
    pub correspondence: RawCorrPhasePayload,
    pub overall: String,
    pub summary: String,
    #[serde(alias = "feedback")]
    pub comments: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CorrLaneMemberInput {
    pub lane_id: LaneId,
    pub ok: bool,
    pub payload: Option<RawCorrPayload>,
    pub error: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CorrNormalizationInput {
    pub request_id: u32,
    pub cycle: u32,
    pub verify_lanes: BTreeSet<LaneId>,
    pub verify_nodes: BTreeSet<NodeId>,
    pub verify_targets: BTreeSet<TargetId>,
    pub members: Vec<CorrLaneMemberInput>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CorrNormalizationOutput {
    pub response: CorrResponse,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RawPaperPayload {
    pub paper_faithfulness: RawCorrPhasePayload,
    pub overall: String,
    pub summary: String,
    #[serde(alias = "feedback")]
    pub comments: String,
}

/// Per-node substantiveness verdict carried by `RawSubstantivenessPhasePayload`.
/// Each verdict carries an explicit `verdict` (Pass / Fail / NotDoneYet) and
/// an optional `comment` (required when `verdict == Fail`, validated by
/// `validate_substantiveness_result_data`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RawSubstantivenessVerdict {
    pub node: String,
    pub verdict: String,
    pub comment: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RawSubstantivenessPhasePayload {
    pub decision: String,
    pub verdicts: Vec<RawSubstantivenessVerdict>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RawDeviationPhasePayload {
    pub id: String,
    pub decision: String,
    pub comment: String,
}

/// Per-node Paper response (substantiveness lane). Distinct from the
/// target-level `RawPaperPayload` shape: carries `verdicts[]` rather than
/// `issues[]`, with explicit per-node Pass / Fail / NotDoneYet.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RawSubstantivenessPayload {
    pub substantiveness: RawSubstantivenessPhasePayload,
    pub overall: String,
    pub summary: String,
    #[serde(alias = "feedback")]
    pub comments: String,
}

/// Combined payload type for the Paper lane. Target-level scenarios
/// populate `paper_faithfulness`; per-node substantiveness scenarios
/// populate `substantiveness`. The kernel cycle scheduler guarantees only
/// one scenario per request, so exactly one block is meaningful per
/// payload (the other is left at `Default::default()`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RawPaperLanePayload {
    /// Target-level (paper-faithfulness) phase block.
    pub paper_faithfulness: RawCorrPhasePayload,
    /// Per-node substantiveness phase block.
    pub substantiveness: RawSubstantivenessPhasePayload,
    /// Single-file deviation authorization block.
    pub deviation_authorization: RawDeviationPhasePayload,
    pub overall: String,
    pub summary: String,
    #[serde(alias = "feedback")]
    pub comments: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PaperLaneMemberInput {
    pub lane_id: LaneId,
    pub ok: bool,
    pub payload: Option<RawPaperLanePayload>,
    pub error: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PaperNormalizationInput {
    pub request_id: u32,
    pub cycle: u32,
    pub verify_lanes: BTreeSet<LaneId>,
    pub verify_targets: BTreeSet<TargetId>,
    /// Substantiveness frontier. Mirrors `verify_nodes` in the
    /// corr normalization input. Exactly one of `verify_targets` /
    /// `verify_nodes` is non-empty per Paper request (the kernel cycle
    /// scheduler picks one scenario per request).
    #[serde(default)]
    pub verify_nodes: BTreeSet<NodeId>,
    #[serde(default)]
    pub verify_deviations: BTreeSet<DeviationId>,
    pub members: Vec<PaperLaneMemberInput>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PaperNormalizationOutput {
    pub response: PaperResponse,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RawSoundnessPayload {
    pub decision: String,
    pub explanation: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RawSoundPayload {
    pub node: String,
    pub soundness: RawSoundnessPayload,
    pub overall: String,
    pub summary: String,
    #[serde(alias = "feedback")]
    pub comments: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SoundLaneMemberInput {
    pub lane_id: LaneId,
    pub ok: bool,
    pub payload: Option<RawSoundPayload>,
    pub error: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SoundNormalizationInput {
    pub request_id: u32,
    pub cycle: u32,
    pub verify_lanes: BTreeSet<LaneId>,
    pub verify_nodes: BTreeSet<NodeId>,
    pub members: Vec<SoundLaneMemberInput>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SoundNormalizationOutput {
    pub response: SoundResponse,
}

pub fn normalize_corr_response(
    input: &CorrNormalizationInput,
) -> Result<CorrNormalizationOutput, String> {
    if input.verify_nodes.is_empty() && input.verify_targets.is_empty() {
        let empty_node_lane_updates: CorrNodeLaneUpdates = input
            .verify_lanes
            .iter()
            .map(|lane| (lane.clone(), BTreeMap::new()))
            .collect();
        let empty_target_lane_updates: CorrTargetLaneUpdates = input
            .verify_lanes
            .iter()
            .map(|lane| (lane.clone(), BTreeMap::new()))
            .collect();
        return Ok(CorrNormalizationOutput {
            response: CorrResponse {
                request_id: input.request_id,
                cycle: input.cycle,
                status: ResponseStatus::Ok,
                node_lane_updates: empty_node_lane_updates,
                target_lane_updates: empty_target_lane_updates,
                reviewer_evidence: BTreeMap::new(),
            },
        });
    }
    let members = collect_members(&input.verify_lanes, &input.members, "correspondence")?;
    let mut node_lane_updates: CorrNodeLaneUpdates = BTreeMap::new();
    let mut target_lane_updates: CorrTargetLaneUpdates = BTreeMap::new();
    let mut reviewer_evidence: BTreeMap<LaneId, CorrReviewerLaneEvidence> = BTreeMap::new();

    for lane in &input.verify_lanes {
        let member = members
            .get(lane)
            .ok_or_else(|| format!("missing correspondence lane {lane}"))?;
        let payload = member
            .payload
            .as_ref()
            .ok_or_else(|| format!("correspondence lane {lane} is missing payload"))?;
        // Build a verdict-by-node map driven by the verifier's explicit
        // `verdicts[]` list. Silence on a node — i.e., the agent didn't emit
        // a verdict for it — defaults to `CorrStatus::Fail` (see
        // `verify_node_status_default` below). This forces the verifier to
        // examine every node it was given; the kernel no longer assumes
        // "no issue mentioned" means Pass.
        //
        // Preamble[<idx>] sub-item verdicts continue to roll up to a single
        // `Preamble` Fail entry, mirroring the legacy `Preamble[` issue-id
        // prefix aggregation.
        let mut verdict_by_node: BTreeMap<String, CorrStatus> = BTreeMap::new();
        let mut comment_by_node: BTreeMap<String, String> = BTreeMap::new();
        let mut preamble_fail_via_subitem = false;
        for verdict in &payload.correspondence.verdicts {
            let node_id = verdict.node.trim();
            if node_id.is_empty() {
                continue;
            }
            let status = match verdict.verdict.trim() {
                "Pass" => CorrStatus::Pass,
                "Fail" => CorrStatus::Fail,
                _ => continue,
            };
            // `Preamble[<idx>]` is a Preamble sub-item finding; aggregate to
            // the canonical `Preamble` node entry. A Fail on any sub-item
            // wins; a Pass-only sub-item set leaves Preamble at whatever
            // top-level verdict was emitted (or default-Fail if none).
            if let Some(suffix) = node_id.strip_prefix(&format!("{PREAMBLE_NAME}[")) {
                if suffix.ends_with(']') && status == CorrStatus::Fail {
                    preamble_fail_via_subitem = true;
                    if !verdict.comment.trim().is_empty() {
                        comment_by_node
                            .entry(PREAMBLE_NAME.to_string())
                            .or_insert_with(|| verdict.comment.trim().to_string());
                    }
                }
                continue;
            }
            verdict_by_node.insert(node_id.to_string(), status);
            if !verdict.comment.trim().is_empty() {
                comment_by_node.insert(node_id.to_string(), verdict.comment.trim().to_string());
            }
        }
        if preamble_fail_via_subitem {
            verdict_by_node.insert(PREAMBLE_NAME.to_string(), CorrStatus::Fail);
        }

        // Synthesize per-node `VerifierIssue` entries from the per-node
        // status map so the reviewer-facing `CorrReviewerPhaseEvidence`
        // (which still carries an `issues[]` field, shared with the
        // paper-faithfulness lane) shows the corr-node failures in the same
        // shape the reviewer prompt has always rendered. Pass verdicts
        // contribute no issue (absence is the "passed" signal). Silent-Fail
        // nodes — i.e., nodes the verifier was asked about but didn't emit
        // a verdict for — must surface as issues too so the reviewer can see
        // them; they get a synthesized "verifier did not return a verdict"
        // description.
        let mut synthesized_issues: Vec<VerifierIssue> = Vec::new();
        for node in &input.verify_nodes {
            let explicit_status = verdict_by_node.get(node.as_str()).copied();
            let status = explicit_status.unwrap_or(CorrStatus::Fail);
            if !matches!(status, CorrStatus::Fail) {
                continue;
            }
            let description = if explicit_status.is_some() {
                comment_by_node
                    .get(node.as_str())
                    .cloned()
                    .filter(|comment| !comment.is_empty())
                    .unwrap_or_else(|| {
                        "verifier did not emit a comment for this Fail verdict".to_string()
                    })
            } else {
                "verifier did not return a verdict for this node".to_string()
            };
            synthesized_issues.push(VerifierIssue {
                node: node.clone(),
                description,
            });
        }
        // Preserve the legacy Preamble[idx] aggregation: a Preamble Fail that
        // came from sub-item rollup gets a single Preamble entry with the
        // first sub-item comment.
        if preamble_fail_via_subitem {
            let already_listed = synthesized_issues
                .iter()
                .any(|issue| issue.node.as_str() == PREAMBLE_NAME);
            if !already_listed {
                let description = comment_by_node
                    .get(PREAMBLE_NAME)
                    .cloned()
                    .filter(|comment| !comment.is_empty())
                    .unwrap_or_else(|| {
                        "verifier did not emit a comment for this Fail verdict".to_string()
                    });
                synthesized_issues.push(VerifierIssue {
                    node: NodeId::from(PREAMBLE_NAME),
                    description,
                });
            }
        }
        synthesized_issues.sort_by(|a, b| a.node.cmp(&b.node));

        reviewer_evidence.insert(
            lane.clone(),
            CorrReviewerLaneEvidence {
                correspondence: CorrReviewerPhaseEvidence {
                    decision: payload.correspondence.decision.clone(),
                    issues: synthesized_issues,
                },
                overall: payload.overall.clone(),
                summary: payload.summary.clone(),
                comments: payload.comments.clone(),
            },
        );

        node_lane_updates.insert(
            lane.clone(),
            input
                .verify_nodes
                .iter()
                .map(|node| {
                    // Default-Fail on silence: the verifier did not emit a
                    // verdict for this node, so we cannot certify it Pass.
                    // Mirrors the substantiveness `NotDoneYet` default but
                    // collapsed to Fail because corr has no third state.
                    let status = verdict_by_node
                        .get(node.as_str())
                        .copied()
                        .unwrap_or(CorrStatus::Fail);
                    (node.clone(), Update::Set(status))
                })
                .collect(),
        );
        target_lane_updates.insert(
            lane.clone(),
            input
                .verify_targets
                .iter()
                .map(|target| (target.clone(), Update::Set(CorrStatus::Pass)))
                .collect(),
        );
    }

    Ok(CorrNormalizationOutput {
        response: CorrResponse {
            request_id: input.request_id,
            cycle: input.cycle,
            status: ResponseStatus::Ok,
            node_lane_updates,
            target_lane_updates,
            reviewer_evidence,
        },
    })
}

pub fn normalize_paper_response(
    input: &PaperNormalizationInput,
) -> Result<PaperNormalizationOutput, String> {
    if input.verify_targets.is_empty()
        && input.verify_nodes.is_empty()
        && input.verify_deviations.is_empty()
    {
        let target_lane_updates = input
            .verify_lanes
            .iter()
            .map(|lane| (lane.clone(), BTreeMap::new()))
            .collect();
        let node_lane_updates: SubstantivenessLaneUpdates = input
            .verify_lanes
            .iter()
            .map(|lane| (lane.clone(), BTreeMap::new()))
            .collect();
        let deviation_lane_updates: DeviationLaneUpdates = input
            .verify_lanes
            .iter()
            .map(|lane| (lane.clone(), BTreeMap::new()))
            .collect();
        return Ok(PaperNormalizationOutput {
            response: PaperResponse {
                request_id: input.request_id,
                cycle: input.cycle,
                status: ResponseStatus::Ok,
                target_lane_updates,
                node_lane_updates,
                deviation_lane_updates,
                reviewer_evidence: BTreeMap::new(),
                node_reviewer_evidence: BTreeMap::new(),
            },
        });
    }

    // Defense-in-depth: enforce that the request payload carries at most one
    // of the three Paper-verifier scenarios (target / per-node / deviation).
    //
    // The Paper request scheduler is supposed to keep these three frontiers
    // mutually exclusive per cycle. The verifier prompt assembly
    // (`paper_scenario_prompt_fragments` in `request_contracts.rs:215-244`)
    // picks ONE scenario via a priority cascade, so the verifier panel sees
    // one set of prompts per request and its response only carries that
    // scenario's payload.
    //
    // If a scheduler regression — or a future code path — ever populated
    // more than one of the request's `verify_*` sets, the normalizer would
    // silently double-process: the per-node verdict bucketer (~line 581)
    // and the deviation-status updater (~line 640) fire on independent
    // flags, so the scenario the verifier did NOT see contributes no
    // verdicts and every node in that scenario's frontier collapses to the
    // `NotDoneYet` default at line 607, corrupting status maps without any
    // surfaced error.
    //
    // Fail loudly at the normalizer boundary instead.
    let scenarios_active = [
        !input.verify_targets.is_empty(),
        !input.verify_nodes.is_empty(),
        !input.verify_deviations.is_empty(),
    ]
    .iter()
    .filter(|x| **x)
    .count();
    if scenarios_active > 1 {
        return Err(format!(
            "paper-faithfulness scenarios are mutually exclusive: \
             verify_targets={} ({}), verify_nodes={} ({}), verify_deviations={} ({})",
            !input.verify_targets.is_empty(),
            input.verify_targets.len(),
            !input.verify_nodes.is_empty(),
            input.verify_nodes.len(),
            !input.verify_deviations.is_empty(),
            input.verify_deviations.len()
        ));
    }

    let members = collect_members(&input.verify_lanes, &input.members, "paper-faithfulness")?;
    let mut target_lane_updates: CorrTargetLaneUpdates = BTreeMap::new();
    let mut node_lane_updates: SubstantivenessLaneUpdates = BTreeMap::new();
    let mut deviation_lane_updates: DeviationLaneUpdates = BTreeMap::new();
    let mut reviewer_evidence: BTreeMap<LaneId, PaperReviewerLaneEvidence> = BTreeMap::new();
    let mut node_reviewer_evidence: BTreeMap<NodeId, BTreeMap<LaneId, PaperReviewerLaneEvidence>> =
        BTreeMap::new();

    let is_per_node_scenario = !input.verify_nodes.is_empty();
    let is_deviation_scenario = !input.verify_deviations.is_empty();

    for lane in &input.verify_lanes {
        let member = members
            .get(lane)
            .ok_or_else(|| format!("missing paper-faithfulness lane {lane}"))?;
        let payload = member
            .payload
            .as_ref()
            .ok_or_else(|| format!("paper-faithfulness lane {lane} is missing payload"))?;
        if is_deviation_scenario {
            let dev = &payload.deviation_authorization;
            let requested = input
                .verify_deviations
                .iter()
                .next()
                .map(|id| id.as_str())
                .unwrap_or("");
            let decision = dev.decision.trim();
            if dev.id.trim().is_empty() || decision.is_empty() {
                return Err(format!(
                    "deviation authorization lane {lane} is missing deviation_authorization"
                ));
            }
            if dev.id.trim() != requested {
                return Err(format!(
                    "deviation authorization lane {lane} returned id {}, expected {}",
                    dev.id.trim(),
                    requested
                ));
            }
            if !matches!(decision, "PASS" | "FAIL") {
                return Err(format!(
                    "deviation authorization lane {lane} decision must be PASS or FAIL"
                ));
            }
        }

        // Reviewer evidence is keyed by the target-level `paper_faithfulness`
        // shape (issues[]). For the per-node scenario we synthesize an
        // equivalent issue list from the verdicts so the reviewer-evidence
        // surface stays uniform: each `Fail` or `NotDoneYet` verdict with
        // a comment becomes an issue entry whose description is the
        // comment. Pass verdicts contribute no per-node evidence.
        let lane_evidence = if is_deviation_scenario {
            let dev = &payload.deviation_authorization;
            let mut issues = Vec::new();
            if dev.decision.trim() != "PASS" {
                issues.push(VerifierIssue {
                    node: NodeId::from(dev.id.trim()),
                    description: dev.comment.trim().to_string(),
                });
            }
            PaperReviewerLaneEvidence {
                paper_faithfulness: CorrReviewerPhaseEvidence {
                    decision: dev.decision.clone(),
                    issues,
                },
                overall: payload.overall.clone(),
                summary: payload.summary.clone(),
                comments: payload.comments.clone(),
            }
        } else if is_per_node_scenario {
            let synthesized_issues: Vec<VerifierIssue> = payload
                .substantiveness
                .verdicts
                .iter()
                .filter_map(|v| {
                    let verdict = v.verdict.trim();
                    let node = v.node.trim();
                    let comment = v.comment.trim();
                    if node.is_empty() {
                        return None;
                    }
                    if verdict == "Pass" {
                        return None;
                    }
                    if comment.is_empty() {
                        return None;
                    }
                    Some(VerifierIssue {
                        node: NodeId::from(node),
                        description: comment.to_string(),
                    })
                })
                .collect();
            PaperReviewerLaneEvidence {
                paper_faithfulness: CorrReviewerPhaseEvidence {
                    decision: payload.substantiveness.decision.clone(),
                    issues: synthesized_issues,
                },
                overall: payload.overall.clone(),
                summary: payload.summary.clone(),
                comments: payload.comments.clone(),
            }
        } else {
            PaperReviewerLaneEvidence {
                paper_faithfulness: CorrReviewerPhaseEvidence {
                    decision: payload.paper_faithfulness.decision.clone(),
                    issues: payload.paper_faithfulness.issues.clone(),
                },
                overall: payload.overall.clone(),
                summary: payload.summary.clone(),
                comments: payload.comments.clone(),
            }
        };
        reviewer_evidence.insert(lane.clone(), lane_evidence.clone());

        // Per-node scenario: bucket verdicts by node id, then map every
        // requested node × lane to a `SubstantivenessStatus` value.
        //
        // The verdict default for missing nodes is `NotDoneYet`. This is
        // a deliberate flip from the previous "implicit Pass" default —
        // the verifier must emit an explicit verdict per node to mark it
        // Pass; silence is treated as triage so accidental omissions
        // don't ratify an unread node.
        if is_per_node_scenario {
            let mut verdict_by_node: BTreeMap<String, SubstantivenessStatus> = BTreeMap::new();
            for verdict in &payload.substantiveness.verdicts {
                let node_id = verdict.node.trim();
                if node_id.is_empty() {
                    continue;
                }
                let status = match verdict.verdict.trim() {
                    "Pass" => SubstantivenessStatus::Pass,
                    "Fail" => SubstantivenessStatus::Fail,
                    "NotDoneYet" => SubstantivenessStatus::NotDoneYet,
                    // Unknown verdict: leave the node out of the lookup
                    // (will fall through to the NotDoneYet default).
                    _ => continue,
                };
                verdict_by_node.insert(node_id.to_string(), status);
            }
            node_lane_updates.insert(
                lane.clone(),
                input
                    .verify_nodes
                    .iter()
                    .map(|node| {
                        let status = verdict_by_node
                            .get(node.as_str())
                            .copied()
                            .unwrap_or(SubstantivenessStatus::NotDoneYet);
                        (node.clone(), Update::Set(status))
                    })
                    .collect(),
            );

            // Aggregate per-node reviewer evidence: every Fail or
            // NotDoneYet verdict with a non-empty comment contributes
            // a per-node entry. Pass verdicts contribute no per-node
            // evidence (lane-level summary suffices). Lane-level
            // evidence is replicated per implicated node so the reviewer
            // sees verifier reasoning alongside the targeted node id.
            for verdict in &payload.substantiveness.verdicts {
                let node_id = verdict.node.trim();
                if node_id.is_empty() {
                    continue;
                }
                let kind = verdict.verdict.trim();
                if kind == "Pass" {
                    continue;
                }
                if verdict.comment.trim().is_empty() {
                    continue;
                }
                node_reviewer_evidence
                    .entry(NodeId::from(node_id))
                    .or_default()
                    .insert(lane.clone(), lane_evidence.clone());
            }
        } else {
            node_lane_updates.insert(lane.clone(), BTreeMap::new());
        }

        if is_deviation_scenario {
            let mut updates = BTreeMap::new();
            for deviation in &input.verify_deviations {
                let decision = payload.deviation_authorization.decision.trim();
                let status = if decision == "PASS" {
                    CorrStatus::Pass
                } else {
                    CorrStatus::Fail
                };
                updates.insert(deviation.clone(), Update::Set(status));
            }
            deviation_lane_updates.insert(lane.clone(), updates);
        } else {
            deviation_lane_updates.insert(lane.clone(), BTreeMap::new());
        }

        // Target scenario continues unchanged: bucket issues by target id
        // and emit Pass/Fail.
        let target_issue_ids = if is_per_node_scenario {
            BTreeSet::new()
        } else {
            normalized_issue_ids(&payload.paper_faithfulness.issues)
        };
        target_lane_updates.insert(
            lane.clone(),
            input
                .verify_targets
                .iter()
                .map(|target| {
                    let status = if target_issue_ids.contains(target.as_str()) {
                        CorrStatus::Fail
                    } else {
                        CorrStatus::Pass
                    };
                    (target.clone(), Update::Set(status))
                })
                .collect(),
        );
    }

    Ok(PaperNormalizationOutput {
        response: PaperResponse {
            request_id: input.request_id,
            cycle: input.cycle,
            status: ResponseStatus::Ok,
            target_lane_updates,
            node_lane_updates,
            deviation_lane_updates,
            reviewer_evidence,
            node_reviewer_evidence,
        },
    })
}

pub fn normalize_sound_response(
    input: &SoundNormalizationInput,
) -> Result<SoundNormalizationOutput, String> {
    if input.verify_nodes.is_empty() {
        let lane_updates = input
            .verify_lanes
            .iter()
            .map(|lane| (lane.clone(), BTreeMap::new()))
            .collect();
        return Ok(SoundNormalizationOutput {
            response: SoundResponse {
                request_id: input.request_id,
                cycle: input.cycle,
                status: ResponseStatus::Ok,
                lane_updates,
                reviewer_evidence: BTreeMap::new(),
            },
        });
    }
    if input.verify_nodes.len() != 1 {
        return Err(
            "sound normalization requires exactly one verify node in the request payload"
                .to_string(),
        );
    }
    let node_name = input
        .verify_nodes
        .iter()
        .next()
        .cloned()
        .ok_or_else(|| "sound normalization requires exactly one verify node".to_string())?;
    let members = collect_members(&input.verify_lanes, &input.members, "soundness")?;
    let mut lane_updates: SoundLaneUpdates = BTreeMap::new();
    let mut reviewer_evidence: BTreeMap<LaneId, SoundReviewerLaneEvidence> = BTreeMap::new();

    for lane in &input.verify_lanes {
        let member = members
            .get(lane)
            .ok_or_else(|| format!("missing soundness lane {lane}"))?;
        let payload = member
            .payload
            .as_ref()
            .ok_or_else(|| format!("soundness lane {lane} is missing payload"))?;
        let payload_node = payload.node.trim();
        if !payload_node.is_empty() && payload_node != node_name.as_str() {
            return Err(format!(
                "soundness lane {lane} reported node {payload_node:?}, expected {node_name:?}"
            ));
        }
        let decision = payload.soundness.decision.trim().to_ascii_uppercase();
        reviewer_evidence.insert(
            lane.clone(),
            SoundReviewerLaneEvidence {
                node: NodeId::from(payload.node.clone()),
                soundness: SoundReviewerDecisionEvidence {
                    decision: payload.soundness.decision.clone(),
                    explanation: payload.soundness.explanation.clone(),
                },
                overall: payload.overall.clone(),
                summary: payload.summary.clone(),
                comments: payload.comments.clone(),
            },
        );
        let status = match decision.as_str() {
            "SOUND" => SoundStatus::Pass,
            "STRUCTURAL" => SoundStatus::Structural,
            _ => SoundStatus::Fail,
        };
        lane_updates.insert(
            lane.clone(),
            BTreeMap::from([(node_name.clone(), Update::Set(status))]),
        );
    }

    Ok(SoundNormalizationOutput {
        response: SoundResponse {
            request_id: input.request_id,
            cycle: input.cycle,
            status: ResponseStatus::Ok,
            lane_updates,
            reviewer_evidence,
        },
    })
}

fn normalized_issue_ids(issues: &[VerifierIssue]) -> BTreeSet<String> {
    issues
        .iter()
        .map(|issue| issue.node.trim())
        .filter(|node| !node.is_empty())
        .map(|node| node.to_string())
        .collect()
}

trait LaneMember {
    fn lane_id(&self) -> &LaneId;
    fn ok(&self) -> bool;
    fn error(&self) -> &str;
}

impl LaneMember for CorrLaneMemberInput {
    fn lane_id(&self) -> &LaneId {
        &self.lane_id
    }

    fn ok(&self) -> bool {
        self.ok
    }

    fn error(&self) -> &str {
        &self.error
    }
}

impl LaneMember for PaperLaneMemberInput {
    fn lane_id(&self) -> &LaneId {
        &self.lane_id
    }

    fn ok(&self) -> bool {
        self.ok
    }

    fn error(&self) -> &str {
        &self.error
    }
}

impl LaneMember for SoundLaneMemberInput {
    fn lane_id(&self) -> &LaneId {
        &self.lane_id
    }

    fn ok(&self) -> bool {
        self.ok
    }

    fn error(&self) -> &str {
        &self.error
    }
}

fn collect_members<'a, T: LaneMember>(
    verify_lanes: &BTreeSet<LaneId>,
    members: &'a [T],
    label: &str,
) -> Result<BTreeMap<LaneId, &'a T>, String> {
    let mut by_lane = BTreeMap::new();
    for member in members {
        let lane = member.lane_id().trim();
        if lane.is_empty() {
            return Err(format!("{label} member is missing lane_id"));
        }
        if !verify_lanes.contains(lane) {
            return Err(format!(
                "{label} member referenced unexpected lane {lane:?}; expected {:?}",
                verify_lanes
            ));
        }
        if by_lane.insert(lane.to_string(), member).is_some() {
            return Err(format!("{label} payload contains duplicate lane {lane:?}"));
        }
    }

    let missing: Vec<_> = verify_lanes
        .iter()
        .filter(|lane| !by_lane.contains_key(*lane))
        .cloned()
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "{label} payload is missing lanes {missing:?}; expected {:?}",
            verify_lanes
        ));
    }

    for lane in verify_lanes {
        let member = by_lane
            .get(lane)
            .ok_or_else(|| format!("missing {label} lane {lane}"))?;
        if !member.ok() {
            let detail = member.error().trim();
            return Err(format!(
                "{label} lane {lane} failed: {}",
                if detail.is_empty() {
                    "missing payload"
                } else {
                    detail
                }
            ));
        }
    }

    Ok(by_lane)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_corr_response_maps_node_failures() {
        let output = normalize_corr_response(&CorrNormalizationInput {
            request_id: 7,
            cycle: 3,
            verify_lanes: BTreeSet::from(["v1".to_string(), "v2".to_string()]),
            verify_nodes: BTreeSet::from([NodeId::from("n1"), NodeId::from("n2")]),
            verify_targets: BTreeSet::from([TargetId::from("target_a")]),
            members: vec![
                CorrLaneMemberInput {
                    lane_id: "v1".to_string(),
                    ok: true,
                    payload: Some(RawCorrPayload {
                        correspondence: RawCorrPhasePayload {
                            verdicts: vec![
                                RawCorrVerdict {
                                    node: "n1".to_string(),
                                    verdict: "Pass".to_string(),
                                    comment: String::new(),
                                },
                                RawCorrVerdict {
                                    node: "n2".to_string(),
                                    verdict: "Fail".to_string(),
                                    comment: "wrong statement".to_string(),
                                },
                            ],
                            ..RawCorrPhasePayload::default()
                        },
                        overall: "REJECT".to_string(),
                        summary: "lane 1 rejects".to_string(),
                        comments: String::new(),
                    }),
                    ..CorrLaneMemberInput::default()
                },
                CorrLaneMemberInput {
                    lane_id: "v2".to_string(),
                    ok: true,
                    payload: Some(RawCorrPayload {
                        correspondence: RawCorrPhasePayload {
                            verdicts: vec![
                                RawCorrVerdict {
                                    node: "n1".to_string(),
                                    verdict: "Pass".to_string(),
                                    comment: String::new(),
                                },
                                RawCorrVerdict {
                                    node: "n2".to_string(),
                                    verdict: "Pass".to_string(),
                                    comment: String::new(),
                                },
                            ],
                            ..RawCorrPhasePayload::default()
                        },
                        ..RawCorrPayload::default()
                    }),
                    ..CorrLaneMemberInput::default()
                },
            ],
        })
        .unwrap();

        assert_eq!(
            output.response.node_lane_updates["v1"]["n1"],
            Update::Set(CorrStatus::Pass)
        );
        assert_eq!(
            output.response.node_lane_updates["v1"]["n2"],
            Update::Set(CorrStatus::Fail)
        );
        assert_eq!(
            output.response.target_lane_updates["v1"]["target_a"],
            Update::Set(CorrStatus::Pass)
        );
        assert_eq!(
            output.response.target_lane_updates["v2"]["target_a"],
            Update::Set(CorrStatus::Pass)
        );
        assert_eq!(
            output.response.node_lane_updates["v2"]["n2"],
            Update::Set(CorrStatus::Pass)
        );
    }

    #[test]
    fn normalize_corr_response_silent_node_defaults_to_fail() {
        // Regression: pre-2026-04-30, the corr lane silently inferred Pass
        // for any node that wasn't mentioned in the issues list. With the
        // verdicts-driven schema, a verifier that names some nodes but
        // omits others must have those omitted ones recorded as Fail —
        // silence is no longer a Pass.
        let output = normalize_corr_response(&CorrNormalizationInput {
            request_id: 11,
            cycle: 4,
            verify_lanes: BTreeSet::from(["v1".to_string()]),
            verify_nodes: BTreeSet::from([
                NodeId::from("alpha"),
                NodeId::from("beta"),
                NodeId::from("gamma"),
            ]),
            verify_targets: BTreeSet::new(),
            members: vec![CorrLaneMemberInput {
                lane_id: "v1".to_string(),
                ok: true,
                payload: Some(RawCorrPayload {
                    correspondence: RawCorrPhasePayload {
                        decision: "FAIL".to_string(),
                        verdicts: vec![RawCorrVerdict {
                            node: "alpha".to_string(),
                            verdict: "Pass".to_string(),
                            comment: String::new(),
                        }],
                        ..RawCorrPhasePayload::default()
                    },
                    ..RawCorrPayload::default()
                }),
                ..CorrLaneMemberInput::default()
            }],
        })
        .unwrap();
        let node_updates = &output.response.node_lane_updates["v1"];
        assert_eq!(node_updates["alpha"], Update::Set(CorrStatus::Pass));
        assert_eq!(node_updates["beta"], Update::Set(CorrStatus::Fail));
        assert_eq!(node_updates["gamma"], Update::Set(CorrStatus::Fail));
        // Synthesized issues should expose the default-Fail nodes for the
        // reviewer with a synthesized "did not emit a comment" description
        // (they didn't get a verdict, so there's no comment to use).
        let issues = &output.response.reviewer_evidence["v1"]
            .correspondence
            .issues;
        assert_eq!(issues.len(), 2);
        let nodes: BTreeSet<_> = issues.iter().map(|i| i.node.clone()).collect();
        assert_eq!(
            nodes,
            BTreeSet::from([NodeId::from("beta"), NodeId::from("gamma")])
        );
    }

    #[test]
    fn normalize_paper_response_rejects_empty_deviation_authorization_block() {
        let err = normalize_paper_response(&PaperNormalizationInput {
            request_id: 12,
            cycle: 5,
            verify_lanes: BTreeSet::from(["v1".to_string()]),
            verify_targets: BTreeSet::new(),
            verify_nodes: BTreeSet::new(),
            verify_deviations: BTreeSet::from([DeviationId::from("dev:a")]),
            members: vec![PaperLaneMemberInput {
                lane_id: "v1".to_string(),
                ok: true,
                payload: Some(RawPaperLanePayload::default()),
                ..PaperLaneMemberInput::default()
            }],
        })
        .expect_err("empty deviation block must not normalize to Fail");

        assert!(err.contains("missing deviation_authorization"));
    }

    #[test]
    fn normalize_corr_response_keeps_empty_target_lane_maps_for_each_lane() {
        let output = normalize_corr_response(&CorrNormalizationInput {
            request_id: 9,
            cycle: 2,
            verify_lanes: BTreeSet::from(["v1".to_string(), "v2".to_string()]),
            verify_nodes: BTreeSet::from([NodeId::from("n1")]),
            verify_targets: BTreeSet::new(),
            members: vec![
                CorrLaneMemberInput {
                    lane_id: "v1".to_string(),
                    ok: true,
                    payload: Some(RawCorrPayload::default()),
                    ..CorrLaneMemberInput::default()
                },
                CorrLaneMemberInput {
                    lane_id: "v2".to_string(),
                    ok: true,
                    payload: Some(RawCorrPayload::default()),
                    ..CorrLaneMemberInput::default()
                },
            ],
        })
        .unwrap();

        assert_eq!(
            output
                .response
                .target_lane_updates
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["v1".to_string(), "v2".to_string()])
        );
        assert!(output.response.target_lane_updates["v1"].is_empty());
        assert!(output.response.target_lane_updates["v2"].is_empty());
    }

    #[test]
    fn normalize_paper_response_maps_target_failures() {
        let output = normalize_paper_response(&PaperNormalizationInput {
            request_id: 7,
            cycle: 3,
            verify_lanes: BTreeSet::from(["v1".to_string(), "v2".to_string()]),
            verify_targets: BTreeSet::from([TargetId::from("target_a")]),
            verify_nodes: BTreeSet::new(),
            verify_deviations: BTreeSet::new(),
            members: vec![
                PaperLaneMemberInput {
                    lane_id: "v1".to_string(),
                    ok: true,
                    payload: Some(RawPaperLanePayload {
                        paper_faithfulness: RawCorrPhasePayload {
                            issues: vec![VerifierIssue {
                                node: NodeId::from("target_a"),
                                description: "coverage mismatch".to_string(),
                            }],
                            ..RawCorrPhasePayload::default()
                        },
                        overall: "REJECT".to_string(),
                        summary: "lane 1 rejects".to_string(),
                        comments: String::new(),
                        ..RawPaperLanePayload::default()
                    }),
                    ..PaperLaneMemberInput::default()
                },
                PaperLaneMemberInput {
                    lane_id: "v2".to_string(),
                    ok: true,
                    payload: Some(RawPaperLanePayload::default()),
                    ..PaperLaneMemberInput::default()
                },
            ],
        })
        .unwrap();

        assert_eq!(
            output.response.target_lane_updates["v1"]["target_a"],
            Update::Set(CorrStatus::Fail)
        );
        assert_eq!(
            output.response.target_lane_updates["v2"]["target_a"],
            Update::Set(CorrStatus::Pass)
        );
    }

    #[test]
    fn normalize_paper_response_per_node_buckets_pass_fail_and_not_done_yet() {
        // Lane v1 fails NodeA (with comment), marks NodeB as NotDoneYet,
        // explicitly Passes NodeC. Lane v2 explicitly Passes A & C but
        // omits NodeB entirely — under the new "missing → NotDoneYet"
        // default, NodeB stays NotDoneYet on v2.
        let output = normalize_paper_response(&PaperNormalizationInput {
            request_id: 11,
            cycle: 5,
            verify_lanes: BTreeSet::from(["v1".to_string(), "v2".to_string()]),
            verify_targets: BTreeSet::new(),
            verify_nodes: BTreeSet::from([
                NodeId::from("NodeA"),
                NodeId::from("NodeB"),
                NodeId::from("NodeC"),
            ]),
            verify_deviations: BTreeSet::new(),
            members: vec![
                PaperLaneMemberInput {
                    lane_id: "v1".to_string(),
                    ok: true,
                    payload: Some(RawPaperLanePayload {
                        substantiveness: RawSubstantivenessPhasePayload {
                            decision: "FAIL".to_string(),
                            verdicts: vec![
                                RawSubstantivenessVerdict {
                                    node: "NodeA".to_string(),
                                    verdict: "Fail".to_string(),
                                    comment: "no paper basis".to_string(),
                                },
                                RawSubstantivenessVerdict {
                                    node: "NodeB".to_string(),
                                    verdict: "NotDoneYet".to_string(),
                                    comment: "didn't have time".to_string(),
                                },
                                RawSubstantivenessVerdict {
                                    node: "NodeC".to_string(),
                                    verdict: "Pass".to_string(),
                                    comment: String::new(),
                                },
                            ],
                        },
                        overall: "REJECT".to_string(),
                        summary: "v1 finds A failing, triages B".to_string(),
                        comments: String::new(),
                        ..RawPaperLanePayload::default()
                    }),
                    ..PaperLaneMemberInput::default()
                },
                PaperLaneMemberInput {
                    lane_id: "v2".to_string(),
                    ok: true,
                    payload: Some(RawPaperLanePayload {
                        substantiveness: RawSubstantivenessPhasePayload {
                            decision: "PASS".to_string(),
                            verdicts: vec![
                                RawSubstantivenessVerdict {
                                    node: "NodeA".to_string(),
                                    verdict: "Pass".to_string(),
                                    comment: String::new(),
                                },
                                RawSubstantivenessVerdict {
                                    node: "NodeC".to_string(),
                                    verdict: "Pass".to_string(),
                                    comment: String::new(),
                                },
                            ],
                        },
                        overall: "APPROVE".to_string(),
                        summary: "v2 sees nothing wrong (omits NodeB)".to_string(),
                        comments: String::new(),
                        ..RawPaperLanePayload::default()
                    }),
                    ..PaperLaneMemberInput::default()
                },
            ],
        })
        .unwrap();

        // v1: explicit verdicts.
        assert_eq!(
            output.response.node_lane_updates["v1"]["NodeA"],
            Update::Set(SubstantivenessStatus::Fail)
        );
        assert_eq!(
            output.response.node_lane_updates["v1"]["NodeB"],
            Update::Set(SubstantivenessStatus::NotDoneYet)
        );
        assert_eq!(
            output.response.node_lane_updates["v1"]["NodeC"],
            Update::Set(SubstantivenessStatus::Pass)
        );
        // v2: explicit Pass on NodeA & NodeC; NodeB omitted → NotDoneYet
        // (the default flip — missing-from-response is no longer Pass).
        assert_eq!(
            output.response.node_lane_updates["v2"]["NodeA"],
            Update::Set(SubstantivenessStatus::Pass)
        );
        assert_eq!(
            output.response.node_lane_updates["v2"]["NodeB"],
            Update::Set(SubstantivenessStatus::NotDoneYet)
        );
        assert_eq!(
            output.response.node_lane_updates["v2"]["NodeC"],
            Update::Set(SubstantivenessStatus::Pass)
        );
        // Per-node reviewer evidence: failures + NotDoneYet (with
        // comments) contribute. Pass verdicts do not.
        assert!(output.response.node_reviewer_evidence.contains_key("NodeA"));
        assert!(output.response.node_reviewer_evidence.contains_key("NodeB"));
        assert!(!output.response.node_reviewer_evidence.contains_key("NodeC"));
    }

    #[test]
    fn normalize_paper_response_per_node_pass_with_comment_does_not_emit_node_evidence() {
        // Pass verdicts must NOT contribute per-node reviewer evidence
        // even when the verifier supplies a comment on Pass — lane
        // summary suffices for Pass nodes.
        let output = normalize_paper_response(&PaperNormalizationInput {
            request_id: 13,
            cycle: 7,
            verify_lanes: BTreeSet::from(["v1".to_string()]),
            verify_targets: BTreeSet::new(),
            verify_nodes: BTreeSet::from([NodeId::from("NodeA")]),
            verify_deviations: BTreeSet::new(),
            members: vec![PaperLaneMemberInput {
                lane_id: "v1".to_string(),
                ok: true,
                payload: Some(RawPaperLanePayload {
                    substantiveness: RawSubstantivenessPhasePayload {
                        decision: "PASS".to_string(),
                        verdicts: vec![RawSubstantivenessVerdict {
                            node: "NodeA".to_string(),
                            verdict: "Pass".to_string(),
                            comment: "looks great".to_string(),
                        }],
                    },
                    overall: "APPROVE".to_string(),
                    summary: "v1 passes A with note".to_string(),
                    comments: String::new(),
                    ..RawPaperLanePayload::default()
                }),
                ..PaperLaneMemberInput::default()
            }],
        })
        .unwrap();
        assert_eq!(
            output.response.node_lane_updates["v1"]["NodeA"],
            Update::Set(SubstantivenessStatus::Pass)
        );
        assert!(output.response.node_reviewer_evidence.is_empty());
    }

    #[test]
    fn normalize_paper_response_per_node_empty_frontier_returns_empty_lane_maps() {
        let output = normalize_paper_response(&PaperNormalizationInput {
            request_id: 12,
            cycle: 6,
            verify_lanes: BTreeSet::from(["v1".to_string()]),
            verify_targets: BTreeSet::new(),
            verify_nodes: BTreeSet::new(),
            verify_deviations: BTreeSet::new(),
            members: vec![],
        })
        .unwrap();

        assert!(output.response.node_lane_updates["v1"].is_empty());
        assert!(output.response.target_lane_updates["v1"].is_empty());
    }

    #[test]
    fn normalize_corr_response_maps_preamble_item_failures_to_preamble() {
        let output = normalize_corr_response(&CorrNormalizationInput {
            request_id: 9,
            cycle: 2,
            verify_lanes: BTreeSet::from(["v1".to_string()]),
            verify_nodes: BTreeSet::from([NodeId::from(PREAMBLE_NAME)]),
            verify_targets: BTreeSet::new(),
            members: vec![CorrLaneMemberInput {
                lane_id: "v1".to_string(),
                ok: true,
                payload: Some(RawCorrPayload {
                    correspondence: RawCorrPhasePayload {
                        verdicts: vec![RawCorrVerdict {
                            node: "Preamble[1]".to_string(),
                            verdict: "Fail".to_string(),
                            comment: "unsupported".to_string(),
                        }],
                        ..RawCorrPhasePayload::default()
                    },
                    ..RawCorrPayload::default()
                }),
                ..CorrLaneMemberInput::default()
            }],
        })
        .unwrap();

        assert_eq!(
            output.response.node_lane_updates["v1"][PREAMBLE_NAME],
            Update::Set(CorrStatus::Fail)
        );
        // The synthesized issue list should reflect the rolled-up Preamble
        // entry, not the raw `Preamble[1]` sub-item key.
        let issues = &output.response.reviewer_evidence["v1"]
            .correspondence
            .issues;
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].node, NodeId::from(PREAMBLE_NAME));
        assert_eq!(issues[0].description, "unsupported");
    }

    #[test]
    fn normalize_sound_response_maps_structural_to_structural_status() {
        let output = normalize_sound_response(&SoundNormalizationInput {
            request_id: 4,
            cycle: 8,
            verify_lanes: BTreeSet::from(["v1".to_string(), "v2".to_string()]),
            verify_nodes: BTreeSet::from([NodeId::from("node_a")]),
            members: vec![
                SoundLaneMemberInput {
                    lane_id: "v1".to_string(),
                    ok: true,
                    payload: Some(RawSoundPayload {
                        node: "node_a".to_string(),
                        soundness: RawSoundnessPayload {
                            decision: "SOUND".to_string(),
                            explanation: "fine".to_string(),
                        },
                        overall: "APPROVE".to_string(),
                        summary: "sound".to_string(),
                        comments: String::new(),
                    }),
                    ..SoundLaneMemberInput::default()
                },
                SoundLaneMemberInput {
                    lane_id: "v2".to_string(),
                    ok: true,
                    payload: Some(RawSoundPayload {
                        node: "node_a".to_string(),
                        soundness: RawSoundnessPayload {
                            decision: "STRUCTURAL".to_string(),
                            explanation: "needs helper".to_string(),
                        },
                        overall: "REJECT".to_string(),
                        summary: "structural".to_string(),
                        comments: String::new(),
                    }),
                    ..SoundLaneMemberInput::default()
                },
            ],
        })
        .unwrap();

        assert_eq!(
            output.response.lane_updates["v1"]["node_a"],
            Update::Set(SoundStatus::Pass)
        );
        assert_eq!(
            output.response.lane_updates["v2"]["node_a"],
            Update::Set(SoundStatus::Structural)
        );
    }

    #[test]
    fn normalize_sound_response_allows_empty_verify_nodes() {
        let output = normalize_sound_response(&SoundNormalizationInput {
            request_id: 5,
            cycle: 9,
            verify_lanes: BTreeSet::from(["v1".to_string(), "v2".to_string()]),
            verify_nodes: BTreeSet::new(),
            members: vec![],
        })
        .unwrap();

        assert_eq!(output.response.lane_updates["v1"], BTreeMap::new());
        assert_eq!(output.response.lane_updates["v2"], BTreeMap::new());
    }

    #[test]
    fn normalize_paper_response_rejects_simultaneous_targets_and_nodes() {
        let err = normalize_paper_response(&PaperNormalizationInput {
            request_id: 1,
            cycle: 1,
            verify_lanes: BTreeSet::from(["v1".to_string()]),
            verify_targets: BTreeSet::from([TargetId::from("target_a")]),
            verify_nodes: BTreeSet::from([NodeId::from("NodeA")]),
            verify_deviations: BTreeSet::new(),
            members: vec![],
        })
        .expect_err("mixed target+node scenarios must be rejected");

        assert!(
            err.contains("mutually exclusive"),
            "expected mutual-exclusion error, got: {err}"
        );
    }

    #[test]
    fn normalize_paper_response_rejects_simultaneous_targets_and_deviations() {
        let err = normalize_paper_response(&PaperNormalizationInput {
            request_id: 2,
            cycle: 1,
            verify_lanes: BTreeSet::from(["v1".to_string()]),
            verify_targets: BTreeSet::from([TargetId::from("target_a")]),
            verify_nodes: BTreeSet::new(),
            verify_deviations: BTreeSet::from([DeviationId::from("dev:a")]),
            members: vec![],
        })
        .expect_err("mixed target+deviation scenarios must be rejected");

        assert!(
            err.contains("mutually exclusive"),
            "expected mutual-exclusion error, got: {err}"
        );
    }

    #[test]
    fn normalize_paper_response_rejects_simultaneous_nodes_and_deviations() {
        let err = normalize_paper_response(&PaperNormalizationInput {
            request_id: 3,
            cycle: 1,
            verify_lanes: BTreeSet::from(["v1".to_string()]),
            verify_targets: BTreeSet::new(),
            verify_nodes: BTreeSet::from([NodeId::from("NodeA")]),
            verify_deviations: BTreeSet::from([DeviationId::from("dev:a")]),
            members: vec![],
        })
        .expect_err("mixed node+deviation scenarios must be rejected");

        assert!(
            err.contains("mutually exclusive"),
            "expected mutual-exclusion error, got: {err}"
        );
    }

    #[test]
    fn normalize_paper_response_rejects_all_three_simultaneously() {
        let err = normalize_paper_response(&PaperNormalizationInput {
            request_id: 4,
            cycle: 1,
            verify_lanes: BTreeSet::from(["v1".to_string()]),
            verify_targets: BTreeSet::from([TargetId::from("target_a")]),
            verify_nodes: BTreeSet::from([NodeId::from("NodeA")]),
            verify_deviations: BTreeSet::from([DeviationId::from("dev:a")]),
            members: vec![],
        })
        .expect_err("all three scenarios populated must be rejected");

        assert!(
            err.contains("mutually exclusive"),
            "expected mutual-exclusion error, got: {err}"
        );
    }
}
