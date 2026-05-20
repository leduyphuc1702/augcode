# JCode PASR v3 Ultra-Compact Plan

> One-file handoff for implementing **Per-Agent, Per-Task Skill Router** in JCode. This compresses the full plan while preserving routing logic, constraints, phases, and acceptance criteria.

## 0. Algorithm invariant

**PASR pipeline:** discover heterogeneous skills -> normalize to `CanonicalSkillManifest` -> create todo tasks -> route per agent and per task -> hard-filter before ranking -> budget context -> progressively load selected skill -> re-route when evidence changes.

**Invariant:** a skill is never globally available just because it exists. It is available only when **source, trust, path, agent, task, permission, risk, dependency and context budget** all allow it.

**Must support:** compatibility-first; per-agent skill surfaces; per-task `candidate/selected/loaded/rejected/blocked` skills; `no-skill`; explicit invocation with authorization; progressive disclosure; ToolGateway as final authority; telemetry for every route decision.

**Skill lifecycle:** `discovered -> normalized -> indexed -> candidate -> selected -> loaded -> used -> evaluated`. A skill may stop at any phase. `selected` means policy-approved for the current task; `loaded` means body/resources entered active context; `used` means the agent followed/cited/invoked it; `evaluated` means outcome telemetry was recorded.

## 1. Non-negotiable rules

Normalize internally, preserve external files. Use JCode overlays instead of modifying third-party skills. Candidate skill at planning time is provisional. Re-route immediately before executing each task. Never load all skill bodies. Never let skill descriptions bypass trust/permission. Never silently merge duplicate names. Never execute untrusted scripts by default. Always allow `no-skill`. Always log route/block/reject reasons.

## 2. Compatibility discovery

**Entrypoint priority:** `SKILL.md > skill.md > Skills.md > skills.md`. If multiple exist in a folder, pick the highest priority and log a warning.

**Skill roots:** `.jcode/skills/**`, `~/.jcode/skills/**`, `.agents/skills/**`, `~/.agents/skills/**`, `.codex/skills/**`, `~/.codex/skills/**`, `.cursor/skills/**`, `~/.cursor/skills/**`, `.claude/skills/**`, `~/.claude/skills/**`, `~/.openhands/skills/installed/**` configurable.

**Nested scope:** `apps/web/.cursor/skills/x/SKILL.md` implies `apps/web/**` unless overridden.

**Context files are not normal skills:** `AGENTS.md`, `CLAUDE.md`, `GEMINI.md`, `.cursor/rules/**`, `.clinerules/**`. Classify as `always_context | conditional_rule | legacy_skill`. Do not blindly inject large context files.

## 3. CanonicalSkillManifest

All adapters normalize into one manifest with these field groups:

```yaml
identity: skillId, name, canonicalName, description
source: sourceKind, sourcePath, skillRoot, discoveredFrom, scopeLevel, scopeDir, entrypointFile
loading: bodyLoadMode, invocationMode, enabled
scope: paths, inferredPaths, triggers, negativeTriggers, languages, frameworks, taskTypes, lifecycleStages, capabilities
permission: allowedAgents, deniedAgents, allowedTools, requiredTools, requiredMcpServers, risk, trustLevel
dependencies: [{type, name, required, description}]
resources: [{path, kind, executable}]
versioning: version, compatibleJcodeVersions, contentHash, lastModified, staleAfterDays
budget: manifestTokens, bodyTokens, resourceTokens
qualitySignals: hasSpecificDescription, hasNegativeScope, hasPathScope, isOverBroad, duplicateGroupId, ambiguityScore
securitySignals: containsPromptInjectionPattern, touchesFilesystem, touchesNetwork, executesShell, requestsSecrets, untrustedScripts
raw: rawFrontmatter, rawOverlay, warnings
```

Allowed enums: `sourceKind = jcode-native | agent-skills | codex | cursor | claude | cline | openhands | legacy-md | unknown-compatible`; `invocationMode = implicit-and-explicit | explicit-only | trigger-only | always-context | disabled`; `trustLevel = system | admin | workspace | repo | user | third-party | unknown`; `risk = low | medium | high | critical | unknown`.

**Mapping rules:** missing/weak description -> infer minimally -> mark low quality -> `explicit-only` by default. `paths = frontmatter + overlay + inferred subtree`; explicit paths + subtree default to intersection unless overlay says union. Cursor `disable-model-invocation` -> `explicit-only`. Codex/OpenAI `allow_implicit_invocation:false` -> `explicit-only`. Skill `allowed-tools` is advisory only; ToolGateway remains authoritative. Preserve unknown frontmatter.

## 4. Adapters and registry

**Adapter contract:** `discover(workspace)`, `canParse(entry)`, `parse(entry)`, `inferScope(entry, workspace)`, `listResources(entry)`.

**Required adapters:** JCodeNative, AgentSkills, Codex, Cursor, Claude, Cline, OpenHands, LegacyMarkdownContext.

**Registry pipeline:** discover candidate files -> pick entrypoint -> parse frontmatter/body boundary -> detect adapter/sourceKind -> normalize manifest -> apply overlays -> estimate tokens -> scan quality/security -> resolve collisions -> build BM25, embedding and metadata indexes.

**Collision policy:** `skillId = sourceKind + scopeLevel + relativePath + name + contentHashPrefix`. Same call name must be disambiguated by source/path and never silently merged. Locality bonus: `subtree > repo > workspace > user > admin > system`; this is a scoring bonus, not automatic selection.

**Ambiguous explicit invocation UX:** if `/skill test` matches multiple manifests, do not guess. Return an ambiguity error with disambiguators: `sourceKind`, `scopeDir`, `relativePath`, `canonicalName`, `skillId`. Support fully-qualified invocation such as `/skill sourceKind:scopeDir:canonicalName` or `/skill skillId`.

**Freshness/versioning:** hash every entrypoint/body/resource. Re-index when hash or mtime changes. Penalize stale skills, unknown compatibility, and missing version metadata; block only if policy requires compatible versions. Version metadata is advisory unless declared by trusted policy.

**Overlays:** `.jcode/skill-overrides.json`, `.jcode/skill-index.json`, `.jcode/skill-policy.json`. Order: `system < admin < workspace < repo < user explicit preference`. Security policy may only tighten unless trusted config explicitly relaxes.

**Policy conflict precedence:** final authority is the most restrictive applicable rule. `ToolGateway deny > policy deny > agent deny > task risk deny > trust/source deny > dependency unavailable > context budget deny > skill advisory allow`. Explicit user invocation may skip only implicit-trigger restrictions; it never bypasses deny, risk, trust, dependency, or ToolGateway checks. Trusted admin/system policy may explicitly relax workspace defaults, but every relaxation must be audited.

## 5. AgentProfile

Each agent owns its own skill/tool surface.

```yaml
required_fields:
  id, role, allowedCapabilities, deniedCapabilities, allowedTools, deniedTools,
  allowedSkillSources, allowedTrustLevels, maxAdvertisedSkills, maxAutoLoadedSkills,
  maxSkillContextTokens, maxSingleSkillTokens, canEdit, canRunShell, canUseNetwork, canReadSecrets
defaults:
  orchestrator_planner: read/search/list only; no edit; few loaded skills
  implementer: read/search/edit/patch/execute; broader skill set
  reviewer: read/search/diff/test command; no edit
  security: read/search/diff only; no network by default
```

## 6. Context model

Layered context: `ConversationContext -> PromptIntentContext -> WorkspaceContext -> EvidenceContext -> TodoPlanContext -> TaskExecutionContext`.

`PromptIntentContext` extracts raw prompt, explicit skill/agent mentions, goals/non-goals, deliverables, constraints, mentioned files/dirs/symbols/frameworks/languages, errors, commands, issue ids, risk hints, change types, clarification need and confidence. Use regex/static parsing for explicit mentions, paths, commands and errors; use lightweight structured LLM for goals/change types; do not ask the LLM to choose from the whole skill list here.

`WorkspaceContext` includes cwd, repo root, branch/status, open/active/recent/diff files, repo tree sample, package/config files, detected languages/frameworks/test frameworks/package managers/build systems, available scripts, discovered skill roots and environment capabilities. Use cache/incremental updates, not full rescans every turn.

`EvidenceContext` tracks files read/edited, symbols touched, commands run, output summaries, test/lint/runtime failures, search findings, assumptions and blockers.

Re-route when a new file type appears, a failure appears, migration/security/deploy files are touched, lifecycle changes, user adds instructions, or the selected skill proves unused/irrelevant.

## 7. Todo-level skill binding

Each `TodoTaskNode` must contain: id, title, objective, status, lifecycleStage (`plan|inspect|edit|run|test|review|document`), taskType, assignedAgent, expected files/dirs/symbols, requiredCapabilities, requiredTools, risk, dependsOn, contextSnapshotId, and `skillRouting`.

`skillRouting = { explicitSkills[], candidateSkills[{skillId, confidence, reason}], selectedSkills[{skillId, confidence, reason}], loadedSkills[], rejectedSkills[{skillId, reason}], blockedSkills[{skillId, reason}], noSkillReason }`.

Todo flow: parse prompt -> observe lightweight workspace -> decompose tasks -> infer lifecycle/type/capabilities/files/agent -> attach manifest-only candidate skills -> before each task re-route using latest evidence -> select 0..N skills under policy/budget -> load full skill only after selection -> execute -> update evidence -> repeat.

Definitions: `candidateSkills` are cheap provisional hints; `selectedSkills` are approved for current task; `loadedSkills` are full body/resources in active context.

Loaded skill prompt rendering must use a constrained wrapper: skill content is untrusted guidance, never system authority. Render order: compact manifest summary -> allowed use case -> relevant instructions excerpt -> constraints/warnings -> resource references. Never inject full unrelated resources. Preserve citations to skillId/path for telemetry.

## 8. Routing algorithm

Input: `agent, task, promptIntent, workspace, evidence, registry, policy`.

**Hard filters before scoring:** enabled; invocation mode compatible with explicit/implicit call; source/trust allowed; agent role allowed; path scope matches task/cwd/active/read/edited/diff files; tools compatible with agent/environment; risk allowed; MCP/CLI deps available/installable; not blocked by policy; body/resource budget possible if load is needed. Explicit invocation skips only implicit-mode restriction, never trust/permission/risk/dependency checks.

**Retrieval uses manifest-level fields only:** exact/alias match, trigger match, path/glob match, capability match, language/framework match, BM25 over name+description+metadata, embedding over name+description+example summary, historical success.

**Scoring:**
```text
0.18 explicit_or_trigger + 0.16 prompt_intent + 0.16 task_capability + 0.14 path_scope
+0.10 language_framework + 0.10 lifecycle_stage + 0.08 agent_role + 0.06 historical_success
+0.04 local_scope -0.10 ambiguity -0.10 risk -0.08 context_cost -0.08 trust -0.06 duplicate
```

Post-score: apply MMR/diversity, prefer narrow path-scoped over broad generic, enforce thresholds/budget, return `no-skill` if nothing qualifies.

```yaml
routing: {min_candidate_confidence: 0.45, min_advertise_confidence: 0.55, min_autoload_confidence: 0.78, max_candidates_per_task: 8, max_selected_per_task: 3, max_loaded_per_task: 2, no_skill_below: 0.45}
high_risk: {require_explicit_or_policy_allow: true, max_loaded_per_task: 1, require_reviewer_agent: true}
context_budget: {max_manifest_context_ratio: 0.02, max_manifest_chars: 8000, max_total_skill_context_ratio: 0.15, max_single_skill_body_tokens: 5000}
```

Budget fallback: drop low-confidence candidates -> prefer local/path-specific skills -> shorten descriptions -> defer full load -> use `no-skill` instead of overbroad skill.

**Learning loop:** persist route outcomes in `SkillOutcomeStore` keyed by `(skillId, agentRole, taskType, lifecycleStage, pathGlob, framework, outcome)`. Reward repeated success; penalize wrong-skill reports, immediate rejection, task failure, or unused loaded skills. Never let historical success override hard filters. In the same conversation, avoid re-selecting a skill that was loaded and unused unless new evidence changes.

## 9. Pseudocode

```text
routeSkillsForTask(input):
  explicit = parseExplicitSkillInvocations(promptIntent, task)
  manifests = registry.listCanonicalManifests()
  hardFiltered, blocked = [], []

  for skill in manifests:
    verdict = hardFilterSkill(skill, agent, task, promptIntent, workspace, evidence, policy, explicit)
    if verdict.allowed: hardFiltered += skill
    else: blocked += {skillId, reason}

  query = buildTaskSkillQuery(task, promptIntent, workspace, evidence)
  retrieved = mergeDedupe(exactNameMatches, triggerMatches, pathMatches, capabilityMatches, bm25Search, embeddingSearch)
  ranked = scoreAndExplain(retrieved)
  diverse = applyDiversityFilter(ranked, maxCandidatesPerTask)
  budgeted = applyContextBudget(diverse, agent, policy)
  selected = budgeted where score >= minAdvertiseConfidence

  if selected empty: return no-skill + blocked
  return candidateSkills + selectedSkills + blockedSkills
```

```text
loadSkillForTask(skillId, agent, task, ctx):
  skill = registry.get(skillId)
  hardFilter again with explicit=[skill.name]
  reject if blocked or over budget
  read skill body
  list resources
  return LoadedSkill
```

```text
createTodoPlan(userPrompt, workspaceRoot):
  promptIntent = extractPromptIntent(userPrompt)
  workspace = observeWorkspaceLight(workspaceRoot)
  tasks = decomposeIntoTasks(promptIntent, workspace)
  for task in tasks:
    infer capabilities, lifecycle, likely files, assignedAgent
    attach manifest-only candidate skills
  return TodoPlanContext
```

## 10. ToolGateway and script policy

Authority order: `skill.requiredTools` = requested/hinted; `agent.allowedTools` = capability; `task.requiredTools` = need; `policy` = workspace permission; `ToolGateway` = final decision.

Before running a skill script: skill trust permits script execution; agent has shell; task needs shell or user/policy approves; script path is inside skill root; script does not access secrets/network unless allowed; command and args are logged.

Default script policy: `system/admin/workspace/repo = allowed_with_agent_permission`; `user = ask_or_policy_allow`; `third_party = deny_by_default`; `unknown = deny`.

## 11. Security

Threats: prompt injection in skill body; retrieval-gaming description; malicious filesystem/network/secret access; typosquatting; duplicate shadowing; abandoned/outdated skill; overbroad false activation.

Scan for: `ignore previous instructions`, `always use this skill`, `send secrets`, `read ~/.ssh`, external `curl`, base64 shell, unexpected `chmod +x`, network calls, secret/env access.

Security scan affects `trustPenalty`, `riskPenalty`, `invocationMode`, `scriptExecutionPolicy`, telemetry warnings. Quarantine options: explicit-only, block entirely, or show manifest but deny body/script. Never silently execute suspicious scripts.

## 12. Telemetry

Log every decision: traceId, timestamp, agentId, taskId, userPromptHash, workspaceHash, promptIntentSummary, taskSummary, candidateSkillIds, selectedSkillIds, loadedSkillIds, blockedSkills with reasons, rejectedSkills with reasons, noSkillReason, scoringBreakdown, tokenBudget, outcome (`taskCompleted`, `testsPassed`, `userAccepted`, `wrongSkillReported`). Never log secrets or raw private file contents.

Lifecycle telemetry: emit events for `discovered`, `normalized`, `indexed`, `candidate`, `selected`, `loaded`, `used`, `evaluated`, each with reason, policy verdict, budget deltas and correlation traceId. Track `selected_not_loaded`, `loaded_not_used`, and `used_failed` as first-class quality signals.

## 13. Evaluation

Metrics: Skill Recall@K, Precision@K, MRR per task, false activation rate, no-skill correctness, wrong-agent skill exposure, path-scope correctness, explicit invocation authorization correctness, context tokens saved vs load-all baseline, task success, test pass, user override.

Required eval cases: planner vs implementer get different candidates; migration task gets db skill and test task gets test skill; Cursor `paths` respected; nested `apps/web` skill scoped correctly; `disable-model-invocation` manual-only; `allow_implicit_invocation:false` respected; minimal Claude/Cline name+description skill works; missing description becomes explicit-only; duplicate names disambiguated; suspicious third-party script blocked; no-skill for simple general coding task; re-route after test failure activates test-debugging.

**Eval fixture schema:**

```yaml
id: string
prompt: string
workspace:
  cwd: string
  files: [{path: string, content: string}]
  activeFiles: [string]
  diffFiles: [string]
agents:
  - id: string
    role: string
    allowedTools: [string]
skills:
  - path: string
    content: string
expected:
  tasks: [{title: string, lifecycleStage: string, taskType: string}]
  candidates: [{task: string, agent: string, skill: string}]
  selected: [{task: string, agent: string, skill: string}]
  loaded: [{task: string, agent: string, skill: string}]
  blocked: [{task: string, agent: string, skill: string, reason: string}]
  noSkill: [{task: string, agent: string, reason: string}]
```

Eval runner must support deterministic mode: embedding disabled, BM25/metadata only, fixed thresholds, snapshot-approved explanations.

## 14. Failure modes and fallback

Registry corrupt or unreadable -> skip registry, warn, route `no-skill`, continue task. Skill parse failure -> create minimal disabled or explicit-only legacy manifest with warning, depending on trust. Embedding unavailable -> BM25 + metadata retrieval only. Token estimate failure -> assume worst-case and defer body load. Dependency probe timeout -> mark dependency unavailable and block only skills requiring it. Policy file invalid -> ignore that layer, log warning, never relax security because of parse failure. Telemetry sink failure -> continue execution with local buffered warning. Repeated router exceptions -> circuit-break PASR for the turn and use no-skill.

## 15. Implementation phases

P1 registry/adapters: scan roots, parse aliases/frontmatter, detect sourceKind, normalize, preserve unknown metadata, overlays, collisions, unit tests. P2 context observer: PromptIntent, Workspace, Evidence, language/framework/package/build detection, path/symbol extraction, caching. P3 todo planner: decompose prompt, assign agent, infer type/stage/capabilities/files, attach provisional candidates. P4 router: hard filters, hybrid retrieval, scoring, MMR, budget, no-skill, explanations. P5 loader/gateway: manifest advertisement, full body on selection, resources on demand, script guardrails, dependency checks. P6 telemetry/eval: traces, fixtures, regression tests, report.

## 16. Completion checklist

Scan `.jcode/.agents/.cursor/.claude/.codex` roots; support all entrypoint aliases; normalize to manifest; preserve unknown frontmatter/source files; respect paths and subtree scoping; respect manual-only fields; support minimal name/description skills; classify repo instruction files as context; extract prompt goals/files/commands/errors/constraints/explicit skills; create todo nodes with stage/capabilities/agent; route independently per task; re-route before execution; return no-skill; keep ToolGateway authoritative; block/quarantine suspicious skills/scripts; log score breakdown and block/reject reasons; run task-level routing eval.

## 17. Do not do

Do not load all skill bodies, force JCode-only metadata, treat repo instructions as normal skills, let descriptions bypass policy, execute untrusted scripts, choose one global skill set for a multi-step prompt, freeze skill choices at planning time, merge duplicate names silently, ask the LLM to select from hundreds of full bodies, or remove `no-skill`.

## 18. Minimal acceptance scenario

Repo: `.agents/skills/db-migration/SKILL.md`; `.cursor/skills/react-component/SKILL.md` with `paths="**/*.tsx"`; `apps/web/.cursor/skills/deploy-web/SKILL.md`; `.claude/skills/test-debugging/SKILL.md`; `.codex/skills/release-notes/SKILL.md` with `allow_implicit_invocation=false`; `AGENTS.md`.

Prompt: `Fix the flaky checkout webhook test, update migration rollback notes if needed, and summarize release notes.`

Expected: todo includes inspect/test, implementation, migration-review and release-notes tasks; planner sees repo/test/debug candidates but not deploy-web unless apps/web is active; implementer sees backend/test/migration only when needed; reviewer sees migration/security/code-review; release-notes not auto-loaded if implicit disabled; react-component appears only for TSX context; AGENTS.md is context, not selected skill; no full skill body loads until selected for current task.

## 19. Next-agent instruction

Inspect JCode repo; locate agent/tool/prompt/context/telemetry/config modules; implement compatibility registry before scoring; implement PromptIntentContext and TodoTaskNode before final router scoring; add per-task routing to todo planner; implement hard filters before semantic retrieval; add progressive loading after manifest routing works; add telemetry from the beginning; build compatibility and task-level eval fixtures; keep source skills unchanged and use overlays for JCode metadata.
