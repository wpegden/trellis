// Included by runtime_cli: filesystem capture, cheap classification and readiness.
use trellis_kernel::closure_identity::{
    Currency, Finding, Identity, OwnerCurrency, RepairRequired,
};

fn emit_repair_required(message: &str) -> std::process::ExitCode {
    let report =
        trellis_kernel::closure_identity::decode_repair(message).expect("typed repair report");
    let _ = serde_json::to_writer_pretty(
        io::stdout(),
        &serde_json::json!({
            "status": "closure_identity_repair_required", "repair": report,
        }),
    );
    println!();
    std::process::ExitCode::from(3)
}

#[derive(Clone)]
struct ClosureContext {
    identity: Identity,
    unavailable: Vec<Finding>,
    lean: bool,
    axcheck_required: bool,
    policy: Result<runtime_cli_observations::ApprovedAxiomsPolicy, String>,
}

fn identity_file(path: &Path, axis: &str, optional: bool, errors: &mut Vec<Finding>) -> String {
    match fs::read(path) {
        Ok(bytes) => hash_bytes(&bytes),
        Err(error) if optional && error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            errors.push(Finding {
                category: Currency::IdentityUnavailable,
                axis: axis.into(),
                recorded: path.display().to_string(),
                expected: error.to_string(),
            });
            String::new()
        }
    }
}

impl ClosureContext {
    fn capture(repo: &Path) -> Self {
        let lean = trellis_kernel::tablet_target_for_repo(repo)
            == trellis_kernel::backend::BackendId::Lean;
        let mut unavailable = Vec::new();
        let (lean_executable_hash, lake_executable_hash, checker_script_hash) =
            local_closure_checker_identities(repo);
        let isabelle_home = isabelle_home_from_env();
        #[cfg(test)]
        let isabelle_home = repo
            .join(".test-isabelle")
            .is_dir()
            .then(|| repo.join(".test-isabelle"))
            .or(isabelle_home);
        let identity = Identity {
            closure_version: CLOSURE_VERSION.into(),
            toolchain_hash: if lean {
                identity_file(
                    &repo.join("lean-toolchain"),
                    "toolchain_hash",
                    false,
                    &mut unavailable,
                )
            } else {
                isabelle_distribution_hash_at(isabelle_home.as_deref())
            },
            lean_executable_hash,
            lake_executable_hash,
            checker_script_hash,
            lake_manifest_hash: identity_file(
                &repo.join(if lean {
                    "lake-manifest.json"
                } else {
                    "isabelle/ROOT"
                }),
                "lake_manifest_hash",
                lean,
                &mut unavailable,
            ),
            preamble_hash: identity_file(
                &repo.join(if lean {
                    "Tablet/Preamble.lean"
                } else {
                    "Tablet/Preamble.thy"
                }),
                "preamble_hash",
                false,
                &mut unavailable,
            ),
        };
        for finding in identity.unavailable(lean) {
            if !unavailable.iter().any(|f| f.axis == finding.axis) {
                unavailable.push(finding);
            }
        }
        Self {
            identity,
            unavailable,
            lean,
            axcheck_required: local_closure_axcheck_required_for_repo(repo),
            policy: runtime_cli_observations::load_approved_axioms_policy(repo),
        }
    }

    fn policy_verdict(
        &self,
        node: &NodeId,
        record: &LocalClosureRecord,
        owner_open: bool,
    ) -> InstalledRecordPolicyVerdict {
        InstalledRecordPolicyVerdict {
            approved_axioms_stale: self.lean
                && self.policy.as_ref().map_or(true, |policy| {
                    let approved = policy.for_node(node.as_str());
                    !record
                        .kernel_axioms
                        .iter()
                        .all(|a| approved.contains(a) || (owner_open && a == "sorryAx"))
                }),
            axcheck_stale: self.lean
                && self.axcheck_required
                && record.axcheck_status != AxcheckStatus::Agreed,
        }
    }

    fn require_available(&self) -> Result<(), String> {
        if self.unavailable.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "IdentityUnavailable: {}",
                serde_json::to_string(&self.unavailable).unwrap()
            ))
        }
    }
}

/// Source and policy are evaluated independently of identity. Cheap mode never
/// opens artifacts; full mode retains the existing certificate admission test.
fn classify_closure_record(
    record: Option<&LocalClosureRecord>,
    node: &NodeId,
    repo: &Path,
    state: &ProtocolState,
    context: &ClosureContext,
    full: bool,
) -> OwnerCurrency {
    let mut result = OwnerCurrency {
        tier: "live".into(),
        node: node.clone(),
        findings: context.unavailable.clone(),
        source_checked: true,
        artifacts_checked: full,
    };
    let Some(record) = record else {
        result.add(
            Currency::Missing,
            "record",
            "absent",
            "eligible owner record",
        );
        return result;
    };
    result.findings.extend(
        context
            .identity
            .compare(record, context.lean)
            .into_iter()
            .filter(|f| f.category != Currency::IdentityUnavailable),
    );
    let source = active_decl_hash_for_node(repo, node.as_str());
    if source.is_empty() || source != record.active_decl_hash {
        result.add(
            Currency::SourceStale,
            "active_decl_hash",
            &record.active_decl_hash,
            &source,
        );
    }
    let statement = active_statement_hash_for_node(repo, node.as_str());
    if statement != record.active_statement_hash {
        result.add(
            Currency::SourceStale,
            "active_statement_hash",
            &record.active_statement_hash,
            statement,
        );
    }
    if record
        .seed_support_binding_is_consistent_with_state(state)
        .is_err()
    {
        result.add(
            Currency::PolicyRejected,
            "seed_support_binding",
            "invalid",
            "current seed binding",
        );
    }
    for (support, expected) in &record.seed_support_file_hashes {
        let path = repo.join("Tablet").join(format!("{support}.lean"));
        if !fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_file())
            || fs::read(&path)
                .map(|b| trellis_kernel::trust_base::raw_sha256(&b))
                .ok()
                .as_ref()
                != Some(expected)
        {
            result.add(
                Currency::SourceStale,
                &format!("seed_support_file:{support}"),
                expected,
                "source binding differs",
            );
        }
    }
    let verdict = context.policy_verdict(node, record, state.live.open_nodes.contains(node));
    if verdict.approved_axioms_stale {
        result.add(
            Currency::PolicyRejected,
            "approved_axioms",
            "observed axioms",
            "current effective policy",
        );
    }
    if verdict.axcheck_stale {
        result.add(
            Currency::PolicyRejected,
            "axcheck_status",
            format!("{:?}", record.axcheck_status),
            "Agreed",
        );
    }
    if let Some(cert) = &record.node_certificate {
        if !cert.roots_are_current() || cert.source_sha256.to_string() != record.active_decl_hash {
            result.add(
                Currency::CorruptEvidence,
                "node_certificate",
                "invalid retained binding",
                "self-consistent certificate",
            );
        }
    }
    if full && !node_certificate_matches_current(record, state, repo) {
        result.add(
            Currency::CorruptEvidence,
            "certificate_admission",
            "failed",
            "current source/artifact/evidence binding",
        );
    }
    if !record_dep_hashes_consistent_with_state(record, state) {
        result.add(
            Currency::CorruptEvidence,
            "dependency_evidence",
            "inconsistent",
            "same-tier evidence",
        );
    }
    result
}

fn classify_current_record(
    record: &LocalClosureRecord,
    repo: &Path,
    state: &ProtocolState,
) -> OwnerCurrency {
    classify_closure_record(
        Some(record),
        &record.node,
        repo,
        state,
        &ClosureContext::capture(repo),
        true,
    )
}

fn validate_probe_identity(repo: &Path, probe: &mut LocalClosureProbeOutput) {
    let context = ClosureContext::capture(repo);
    if !context.lean {
        return;
    }
    // Synthetic probes in unit tests predate the transport handshake. Explicit
    // test identities still take exactly the production comparison path.
    #[cfg(test)]
    if probe.checker_identity.is_none() {
        return;
    }
    if context.require_available().is_err()
        || probe.checker_identity.as_ref() != Some(&context.identity)
    {
        probe.status = "identity_unavailable".into();
        probe.errors.push(format!(
            "checker execution identity differs from required identity: expected={} actual={}",
            serde_json::to_string(&context.identity).unwrap(),
            serde_json::to_string(&probe.checker_identity).unwrap()
        ));
    }
}

fn probe_identity_matches_record(
    probe: &LocalClosureProbeOutput,
    record: &LocalClosureRecord,
    repo: &Path,
) -> bool {
    if trellis_kernel::tablet_target_for_repo(repo) != trellis_kernel::backend::BackendId::Lean {
        return true;
    }
    #[cfg(test)]
    if probe.checker_identity.is_none() {
        return true;
    }
    probe.checker_identity.as_ref() == Some(&Identity::of(record))
}

fn quote_shell(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn repair_report(owners: Vec<OwnerCurrency>, state: &ProtocolState, root: &Path) -> RepairRequired {
    let direct_stale: BTreeSet<NodeId> = owners
        .iter()
        .filter(|r| r.repair_required())
        .map(|r| r.node.clone())
        .collect();
    let mut expanded_stale = direct_stale.clone();
    let mut reverse: BTreeMap<NodeId, BTreeSet<NodeId>> = BTreeMap::new();
    for records in [
        &state.local_closure_records,
        &state.committed_local_closure_records,
        &state.last_clean_local_closure_records,
    ] {
        for (consumer, record) in records {
            for provider in record.authoritative_dependency_owners(state) {
                reverse
                    .entry(provider)
                    .or_default()
                    .insert(consumer.clone());
            }
        }
    }
    let mut pending: std::collections::VecDeque<_> = expanded_stale.iter().cloned().collect();
    while let Some(provider) = pending.pop_front() {
        for consumer in reverse.get(&provider).into_iter().flatten() {
            if expanded_stale.insert(consumer.clone()) {
                pending.push_back(consumer.clone());
            }
        }
    }
    let cli = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("trellis_runtime_cli"));
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/migrate_local_closure_records");
    RepairRequired {
        owners,
        direct_stale,
        expanded_stale,
        repair_command: format!(
            "{} {} --mode identity-only --apply --runtime-cli {}",
            quote_shell(&script.display().to_string()),
            quote_shell(&root.display().to_string()),
            quote_shell(&cli.display().to_string())
        ),
    }
}

fn require_identity_currency(
    state: &ProtocolState,
    repo: &Path,
    root: &Path,
) -> Result<(), RuntimeError> {
    if !repo.join("Tablet").is_dir() {
        return Ok(());
    }
    let context = ClosureContext::capture(repo);
    let owners: Vec<_> = state
        .live
        .present_nodes
        .iter()
        .filter(|n| state.local_closure_owner_eligible(n))
        .map(|n| {
            classify_closure_record(
                state.local_closure_records.get(n),
                n,
                repo,
                state,
                &context,
                false,
            )
        })
        .filter(|r| r.repair_required())
        .collect();
    if owners.is_empty() {
        Ok(())
    } else {
        Err(RuntimeError::ClosureIdentityRepairRequired(repair_report(
            owners, state, root,
        )))
    }
}

fn require_runtime_identity(runtime: &SupervisorRuntime) -> Result<(), String> {
    let Some(repo) = runtime.metadata().repo_path.as_deref() else {
        return Ok(());
    };
    let mut report = plan_runtime_identity(runtime.state(), repo, &runtime.paths().root);
    if runtime.metadata().local_closure_initial_issuance_pending {
        // Genesis may still need its first records. An existing record never
        // receives that exemption, even during a partially completed sweep.
        let owners = report
            .owners
            .into_iter()
            .filter(|r| {
                !r.findings
                    .iter()
                    .any(|f| f.category == Currency::Missing && f.axis == "record")
            })
            .collect();
        report = repair_report(owners, runtime.state(), &runtime.paths().root);
    }
    if report.direct_stale.is_empty() {
        Ok(())
    } else {
        Err(RuntimeError::ClosureIdentityRepairRequired(report).to_string())
    }
}

// A historical tier's Preamble is part of its paired source view. Global
// executables and manifest remain those of the environment being activated.
// Source is fully checked by the existing post-restore admission guard.
fn git_policy_file(repo: &Path, commit: &str, path: &str) -> Result<Option<String>, String> {
    let entries = Command::new("git")
        .current_dir(repo)
        .args(["ls-tree", "--name-only", commit, "--", path])
        .output()
        .map_err(|e| e.to_string())?;
    if !entries.status.success() {
        return Err(format!("cannot inspect {commit}:{path}"));
    }
    if entries.stdout.is_empty() {
        return Ok(None);
    }
    let file = Command::new("git")
        .current_dir(repo)
        .args(["show", &format!("{commit}:{path}")])
        .output()
        .map_err(|e| e.to_string())?;
    if !file.status.success() {
        return Err(format!("cannot read {commit}:{path}"));
    }
    String::from_utf8(file.stdout)
        .map(Some)
        .map_err(|e| e.to_string())
}

fn capture_git_policy(
    context: &mut ClosureContext,
    repo: &Path,
    commit: &str,
) -> Result<(), String> {
    let raw = git_policy_file(repo, commit, "APPROVED_AXIOMS.json")?;
    let proposed = git_policy_file(repo, commit, "PROPOSED_ASSUMPTIONS.json")?;
    let proposed: trellis_kernel::assumptions_registry::ProposedAssumptions = match proposed {
        Some(raw) if !raw.trim().is_empty() => {
            serde_json::from_str(&raw).map_err(|e| e.to_string())?
        }
        _ => Default::default(),
    };
    let pending = proposed
        .pending()
        .map(|a| a.axiom_name.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    context.policy =
        runtime_cli_observations::approved_axioms_policy_from_sources(raw.as_deref(), pending);
    let config = match git_policy_file(repo, commit, "trellis.config.json")? {
        Some(raw) => Some(raw),
        None => git_policy_file(repo, commit, "lagent.config.json")?,
    };
    // Match the ordinary resolver's conservative true on absent/malformed config.
    context.axcheck_required = config
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| {
            v.get("local_closure_axcheck_enabled")
                .and_then(|v| v.as_bool())
        })
        .unwrap_or(true);
    Ok(())
}

fn plan_runtime_identity(state: &ProtocolState, repo: &Path, root: &Path) -> RepairRequired {
    let context = ClosureContext::capture(repo);
    let mut owners = Vec::new();
    for (tier, snapshot, kinds, proofs, records) in [
        (
            "live",
            &state.live,
            &state.node_kinds,
            &state.proof_nodes,
            &state.local_closure_records,
        ),
        (
            "committed",
            &state.committed,
            &state.committed_node_kinds,
            &state.committed_proof_nodes,
            &state.committed_local_closure_records,
        ),
        (
            "last_clean",
            &state.last_clean_live,
            &state.last_clean_node_kinds,
            &state.last_clean_proof_nodes,
            &state.last_clean_local_closure_records,
        ),
    ] {
        if tier == "last_clean" && !state.last_clean_local_closure_mirror_ready {
            continue;
        }
        let last_clean_source = if tier == "last_clean" {
            Some(trellis_kernel::runtime::resolve_last_clean_source(repo, state.last_clean_commit.as_deref()))
        } else { None };
        let mut tier_context = context.clone();
        let mut identity = context.identity.clone();
        let mut unavailable = context.unavailable.clone();
        let worker_base = root.join("active_worker_base/Tablet");
        let worker = state
            .in_flight_request
            .as_ref()
            .is_some_and(|r| r.kind == RequestKind::Worker);
        let paired_preamble = if tier != "last_clean" && worker {
            Some(
                fs::read(worker_base.join(if context.lean {
                    "Preamble.lean"
                } else {
                    "Preamble.thy"
                }))
                .map(|b| hash_bytes(&b))
                .map_err(|e| e.to_string()),
            )
        } else if tier == "last_clean" {
            last_clean_source.as_ref().map(|source| {
                let commit = source.as_ref().map_err(|e| e.to_string())?;
                let path = if context.lean {
                    "Tablet/Preamble.lean"
                } else {
                    "Tablet/Preamble.thy"
                };
                Command::new("git")
                    .current_dir(repo)
                    .args(["show", &format!("{commit}:{path}")])
                    .output()
                    .map_err(|e| e.to_string())
                    .and_then(|o| {
                        if o.status.success() {
                            Ok(hash_bytes(&o.stdout))
                        } else {
                            Err(String::from_utf8_lossy(&o.stderr).into_owned())
                        }
                    })
            })
        } else {
            None
        };
        if let Some(paired) = paired_preamble {
            unavailable.retain(|f| f.axis != "preamble_hash");
            match paired {
                Ok(hash) => identity.preamble_hash = hash,
                Err(error) => unavailable.push(Finding {
                    category: Currency::IdentityUnavailable,
                    axis: "paired_preamble".into(),
                    recorded: tier.into(),
                    expected: error,
                }),
            }
        }
        // HEAD/LastClean restores also replace tracked global pins. Inspect
        // that Git view rather than comparing historical records to live WIP.
        // If the pin selects another executable generation, fail closed until
        // that view can be resolved in maintenance; never label live binaries
        // as the executable identity of a different historical toolchain.
        let commit = if tier == "committed" && !worker {
            Some("HEAD")
        } else if tier == "last_clean" {
            last_clean_source.as_ref().and_then(|r| r.as_ref().ok()).map(String::as_str)
        } else {
            None
        };
        if let Some(commit) = commit.filter(|_| repo.join(".git").exists()) {
            if let Err(error) = capture_git_policy(&mut tier_context, repo, commit) {
                tier_context.policy = Err(error);
            }
            for (axis, path, optional) in [
                (
                    "preamble_hash",
                    if context.lean {
                        "Tablet/Preamble.lean"
                    } else {
                        "Tablet/Preamble.thy"
                    },
                    false,
                ),
                ("toolchain_hash", "lean-toolchain", false),
                (
                    "lake_manifest_hash",
                    if context.lean {
                        "lake-manifest.json"
                    } else {
                        "isabelle/ROOT"
                    },
                    context.lean,
                ),
            ] {
                if axis == "toolchain_hash" && !context.lean {
                    continue;
                }
                let exists = Command::new("git")
                    .current_dir(repo)
                    .args(["ls-tree", "--name-only", commit, "--", path])
                    .output();
                let resolved = match exists {
                    Ok(o) if o.status.success() && o.stdout.is_empty() && optional => {
                        Ok(String::new())
                    }
                    Ok(o) if o.status.success() && !o.stdout.is_empty() => Command::new("git")
                        .current_dir(repo)
                        .args(["show", &format!("{commit}:{path}")])
                        .output()
                        .map_err(|e| e.to_string())
                        .and_then(|o| {
                            if o.status.success() {
                                Ok(hash_bytes(&o.stdout))
                            } else {
                                Err(String::from_utf8_lossy(&o.stderr).into_owned())
                            }
                        }),
                    _ => Err(format!(
                        "required paired identity {commit}:{path} unavailable"
                    )),
                };
                match resolved {
                    Ok(value) => {
                        unavailable.retain(|f| f.axis != axis);
                        match axis {
                            "preamble_hash" => identity.preamble_hash = value,
                            "lake_manifest_hash" => identity.lake_manifest_hash = value,
                            _ => {
                                if value != identity.toolchain_hash {
                                    unavailable.push(Finding {
                                        category: Currency::IdentityUnavailable,
                                        axis: "paired_executables".into(),
                                        recorded: commit.into(),
                                        expected:
                                            "resolve the historical toolchain before activation"
                                                .into(),
                                    });
                                }
                                identity.toolchain_hash = value;
                            }
                        }
                    }
                    Err(error) => unavailable.push(Finding {
                        category: Currency::IdentityUnavailable,
                        axis: axis.into(),
                        recorded: format!("{commit}:{path}"),
                        expected: error,
                    }),
                }
            }
        }
        for node in &snapshot.present_nodes {
            if state.trust_base.seed_support_definitions.contains_key(node)
                || matches!(
                    state.node_role.get(node),
                    Some(trellis_kernel::PvRole::UnderModelAssumptions)
                )
                || !(proofs.contains(node)
                    || matches!(
                        kinds.get(node),
                        Some(NodeKind::Proof | NodeKind::Definition | NodeKind::Preamble)
                    ))
            {
                continue;
            }
            let mut r = OwnerCurrency {
                tier: tier.into(),
                node: node.clone(),
                findings: unavailable.clone(),
                source_checked: false,
                artifacts_checked: false,
            };
            if let Some(record) = records.get(node) {
                r.findings.extend(
                    identity
                        .compare(record, context.lean)
                        .into_iter()
                        .filter(|f| f.category != Currency::IdentityUnavailable),
                );
                let verdict =
                    tier_context.policy_verdict(node, record, snapshot.open_nodes.contains(node));
                if verdict.approved_axioms_stale {
                    r.add(
                        Currency::PolicyRejected,
                        "approved_axioms",
                        "observed axioms",
                        "current effective policy",
                    );
                }
                if verdict.axcheck_stale {
                    r.add(
                        Currency::PolicyRejected,
                        "axcheck_status",
                        format!("{:?}", record.axcheck_status),
                        "Agreed",
                    );
                }
            } else {
                r.add(
                    Currency::Missing,
                    "record",
                    "absent",
                    "eligible owner record",
                );
            }
            if r.repair_required() {
                owners.push(r);
            }
        }
    }
    repair_report(owners, state, root)
}

#[cfg(test)]
mod closure_policy_view_tests {
    use super::*;
    #[test]
    fn historical_policy_uses_the_restored_git_view() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        fs::write(
            repo.join("APPROVED_AXIOMS.json"),
            r#"{"global":["Old.axiom"]}"#,
        )
        .unwrap();
        fs::write(
            repo.join("trellis.config.json"),
            r#"{"local_closure_axcheck_enabled":false}"#,
        )
        .unwrap();
        for args in [
            vec!["init"],
            vec!["add", "-A"],
            vec![
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-m",
                "paired policy",
            ],
        ] {
            assert!(Command::new("git")
                .current_dir(repo)
                .args(args)
                .output()
                .unwrap()
                .status
                .success());
        }
        fs::write(repo.join("APPROVED_AXIOMS.json"), "[]").unwrap();
        fs::write(
            repo.join("trellis.config.json"),
            r#"{"local_closure_axcheck_enabled":true}"#,
        )
        .unwrap();
        let mut context = ClosureContext::capture(repo);
        assert!(!context
            .policy
            .as_ref()
            .unwrap()
            .for_node("A")
            .contains("Old.axiom"));
        assert!(context.axcheck_required);
        capture_git_policy(&mut context, repo, "HEAD").unwrap();
        assert!(context
            .policy
            .as_ref()
            .unwrap()
            .for_node("A")
            .contains("Old.axiom"));
        assert!(!context.axcheck_required);
        assert_eq!(
            fs::read_to_string(repo.join("APPROVED_AXIOMS.json")).unwrap(),
            "[]"
        );
    }
}
