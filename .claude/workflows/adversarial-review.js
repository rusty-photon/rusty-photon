export const meta = {
  name: 'adversarial-review',
  description: 'One adversarial review round on a PR: path-gated specialist lenses, skeptics that try to refute every finding, and a ready-to-post GitHub review',
  whenToUse: 'Reviewing a rusty-photon pull request, standalone or while babysitting. Args: a PR number ("1458", "1458 full"), or {pr, full?, round?, since?, skip?}. Run from a checkout of the PR head. Process: docs/skills/adversarial-review.md',
  phases: [
    { title: 'Scope', detail: 'PR purpose, files, delta since the last reviewed head' },
    { title: 'Review', detail: 'path-gated specialist lenses in parallel' },
    { title: 'Dedupe', detail: 'merge findings that name the same defect' },
    { title: 'Verify', detail: 'skeptics try to refute each finding' },
    { title: 'Settle', detail: 'checkout and PR head unchanged; which findings sit on lines of the PR diff' },
  ],
}

// ---------------------------------------------------------------- arguments

let A = args
if (A === undefined || A === null || typeof A === 'number' || typeof A === 'string') {
  const toks = String(A ?? '').trim().split(/\s+/)
  A = { pr: toks[0], full: toks.includes('full') }
}
const pr = Number(String(A.pr ?? '').trim().replace(/^#/, ''))
if (!Number.isInteger(pr) || pr <= 0) {
  throw new Error('adversarial-review: pass a PR number ("1458", "1458 full") or {pr, full?, round?, since?, skip?}')
}
const skip = Array.isArray(A.skip) ? A.skip : []

// ------------------------------------------------------------------- lenses

const LOCKFILE = /(^|\/)(Cargo\.lock|MODULE\.bazel\.lock)$/
const CODE = /\.(rs|sh|py|ps1|js|mjs|ts)$/
// Scripts are routed by directory, not extension: packaging scriptlets
// (`postinst.common`), hooks and workflow `run:` blocks have none of the
// extensions above.
const SCRIPTS = /^packaging\/|(^|\/)pkg\/|^\.cargo-husky\/hooks\/|^scripts\/|^tools\/|^\.github\/(workflows|actions)\//
const CI = /^\.github\/(workflows|actions)\/|^\.github\/(dependabot\.yml|actionlint\.yaml)$|^scripts\/|^tools\/|^installer\/|^packaging\/|(^|\/)pkg\/|^\.cargo-husky\/|^\.cargo\/|^third_party\/|(^|\/)BUILD\.bazel$|^MODULE\.bazel$|\.bzl$|^\.bazel(rc|version|ignore)$|(^|\/)Cargo\.toml$/
const DOCS = /\.md$/
const SERVICE_RUST = /^services\/.*\.rs$/
const TESTS = /\.(rs|feature)$/

// Correctness and safety take every non-markdown file, so no reviewable
// file can fall through every lens and leave a round "quiet" unread.
const LENSES = [
  { key: 'concurrency', agentType: 'review-concurrency', applies: f => CODE.test(f) || SCRIPTS.test(f) },
  { key: 'correctness', agentType: 'review-correctness', applies: f => !DOCS.test(f) },
  { key: 'safety', agentType: 'review-safety', applies: f => !DOCS.test(f) },
  { key: 'tests', agentType: 'review-tests', applies: f => TESTS.test(f) },
  { key: 'silent-failures', agentType: 'pr-review-toolkit:silent-failure-hunter', plugin: true, applies: f => CODE.test(f) || SCRIPTS.test(f) },
  { key: 'ci-packaging', agentType: 'review-ci-packaging', applies: f => CI.test(f) },
  { key: 'docs', agentType: 'review-docs', applies: f => DOCS.test(f) || SERVICE_RUST.test(f) },
]

const PLUGIN_INSTALL = 'claude plugin install pr-review-toolkit@claude-plugins-official --scope project'

// Pinned, not inherited: a round must review with the same strength
// whatever model and effort the session that runs it is set to. Every
// review body states them.
const MODEL = 'opus'
// Lenses cast a wide net and skeptics prune it, so rigor buys the most
// at the skeptic: a lens that over-reports costs a skeptic run, while a
// skeptic that lets a wrong finding through costs a maintainer.
const LENS_EFFORT = 'high'
const VERIFY_EFFORT = 'xhigh'
const CHORE_EFFORT = 'low' // scope, dedupe, settle: run given commands, no judgement
const LENS = { model: MODEL, effort: LENS_EFFORT }
const VERIFY = { model: MODEL, effort: VERIFY_EFFORT }
const CHORE = { model: MODEL, effort: CHORE_EFFORT }

// The plugin agent is written for any codebase; this narrows it to the
// failures that matter here and keeps it off the categories the record
// rates lowest.
const PLUGIN_BRIEF = [
  'This repository is rusty-photon: Rust services that drive telescopes, cameras and focusers unattended overnight.',
  'Report only failures whose masking produces a wrong action, wrong data, or a wedged state: an error converted into a plausible value, a fallback that hides a device or config failure, an error flattened so callers lose the variant they switch on.',
  'Do not ask for extra logging, friendlier messages, or broader error types for their own sake. Do not report style.',
  'Before reviewing, read docs/skills/adversarial-review.md section "Ground rules" and follow them. Never modify the checkout.',
].join(' ')

// ------------------------------------------------------------------ schemas

const SCOPE = {
  type: 'object',
  properties: {
    ok: { type: 'boolean' },
    error: { type: 'string' },
    title: { type: 'string' },
    state: { type: 'string' },
    base_ref: { type: 'string' },
    purpose: { type: 'string', description: '2-4 sentences: the change the PR makes and the problem it solves' },
    head_sha: { type: 'string' },
    merge_base: { type: 'string' },
    files: { type: 'array', items: { type: 'string' } },
    last_round: { type: 'integer', description: 'highest round=N in any marker, 0 if none' },
    last_head: { type: 'string', description: 'head=SHA from the newest marker that has one, empty if none' },
    since: { type: 'string', description: 'the delta base actually used, empty if none' },
    since_reachable: { type: 'boolean' },
    delta_files: { type: 'array', items: { type: 'string' } },
    prior: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          id: { type: 'string' },
          file: { type: 'string' },
          line: { type: 'integer' },
          title: { type: 'string' },
          outcome: { type: 'string', enum: ['fixed', 'declined', 'open', 'unknown'] },
          note: { type: 'string' },
        },
        required: ['id', 'title', 'outcome'],
      },
    },
  },
  required: ['ok'],
}

const FINDINGS = {
  type: 'object',
  properties: {
    findings: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          title: { type: 'string', description: 'one line naming the defect' },
          file: { type: 'string', description: 'repo-relative path' },
          line: { type: 'integer', description: '1-based line number at the PR head' },
          severity: { type: 'string', enum: ['high', 'medium', 'low'] },
          trigger: { type: 'string', description: 'the input, state or interleaving that causes it' },
          consequence: { type: 'string', description: 'what goes wrong, concretely' },
          evidence: { type: 'string', description: 'code pointers (path:line) and command output supporting it' },
          remedy: { type: 'string', description: 'suggested fix, if any' },
          also_at: { type: 'array', items: { type: 'string' }, description: 'sibling sites (path:line) with the same defect' },
        },
        required: ['title', 'file', 'line', 'severity', 'trigger', 'consequence', 'evidence'],
      },
    },
    omitted: { type: 'integer', description: 'findings dropped to stay within the cap' },
  },
  required: ['findings'],
}

const GROUPS = {
  type: 'object',
  properties: {
    groups: { type: 'array', items: { type: 'array', items: { type: 'integer' } } },
  },
  required: ['groups'],
}

const VERDICT = {
  type: 'object',
  properties: {
    verdict: { type: 'string', enum: ['confirmed', 'refuted', 'pre_existing'] },
    reasoning: { type: 'string' },
    evidence: { type: 'string', description: 'what you read or ran: path:line, command output' },
    remedy_note: { type: 'string', description: 'set when the suggested remedy is wrong or incomplete' },
    statement_note: { type: 'string', description: 'set when the defect is real but this statement overstates its severity, trigger or consequence: say what is overstated' },
  },
  required: ['verdict', 'reasoning'],
}

const SETTLE = {
  type: 'object',
  properties: {
    checkout_head: { type: 'string', description: 'what git rev-parse HEAD prints now' },
    checkout_dirty: { type: 'boolean', description: 'whether git status --porcelain --untracked-files=no printed anything' },
    pr_head: { type: 'string', description: 'the PR headRefOid now' },
    anchors: {
      type: 'array',
      items: {
        type: 'object',
        properties: { index: { type: 'integer' }, in_diff: { type: 'boolean' } },
        required: ['index', 'in_diff'],
      },
    },
  },
  required: ['checkout_head', 'checkout_dirty', 'pr_head', 'anchors'],
}

// ------------------------------------------------------------------ helpers

const tick = s => '`' + s + '`'
const short = s => String(s || '').slice(0, 8)
const SHA = /^[0-9a-f]{40}$/
const SEV_RANK = { high: 0, medium: 1, low: 2 }
const clip = (s, n) => {
  const t = String(s || '').replace(/\s+/g, ' ').trim()
  return t.length > n ? t.slice(0, n - 1) + '…' : t
}

// The repo is public: anyone can post a review or a comment, so only
// rounds and outcomes from accounts with write access count. That is the
// push-permission collaborator list, not author_association: MEMBER and
// COLLABORATOR also cover read- and triage-only accounts.
const WRITERS = `W=$(gh api --paginate 'repos/{owner}/{repo}/collaborators?permission=push' | jq -s -c '[.[][].login]') && [ "$W" != "[]" ]`
const TRUSTED = 'select(.user.login as $l | $w | index($l))'

// ==================================================================== Scope

phase('Scope')
const scope = await agent(
  [
    `You are the scope stage of an adversarial review of PR #${pr}. Run the commands below and report what they print. Do not review anything, and never modify the checkout beyond a git fetch.`,
    '',
    `1. gh pr view ${pr} --json title,body,state,baseRefName,headRefOid`,
    '   Report title, state and base_ref (= baseRefName). head_sha = headRefOid.',
    '   Write purpose: 2-4 sentences on the change the PR makes and the problem it solves, from the title, body and `git diff --stat`.',
    '2. git rev-parse HEAD. It MUST equal headRefOid. If it does not, return ok=false with',
    `   error = "checkout HEAD <sha> is not the head of PR #${pr} (<headRefOid>): check out the PR head and re-run". Stop there.`,
    '   git status --porcelain --untracked-files=no must print nothing. If it prints anything, return ok=false with',
    '   error = "the checkout has uncommitted changes: a round reads the working tree, so commit them or wait, then re-run". Stop there.',
    '3. git fetch origin <baseRefName> --quiet, then merge_base = git merge-base origin/<baseRefName> HEAD.',
    '   files = every path `git diff --name-only <merge_base> HEAD` prints. Do not take files from gh pr view: its list stops at 100.',
    '4. Previous rounds. Each adversarial-review round left a marker at the end of its review body:',
    '   "<!-- adversarial-review round=N -->" or "<!-- adversarial-review round=N head=SHA -->". Read them with exactly this command,',
    '   which keeps only reviews by accounts with write access and takes the last marker in each body:',
    `   set -o pipefail; ${WRITERS} && gh api --paginate 'repos/{owner}/{repo}/pulls/${pr}/reviews' | jq -s -r --argjson w "$W" '.[][] | ${TRUSTED} | select(.body | test("<!-- adversarial-review round=")) | "\\(.id)\\t\\([.body | match("<!-- adversarial-review ([^>]*)-->"; "g")] | last | .captures[0].string)"'`,
    '   If it exits non-zero or prints an error, return ok=false naming step 4: a failed read must never look like "no previous rounds".',
    '   last_round = the highest round=N it prints (0 only when it succeeded and printed nothing). last_head = the head=SHA of the newest line that has one ("" if none).',
    '5. Delta base: ' + (A.full
      ? 'none — this is a full review; since = "".'
      : (A.since
        ? `use ${A.since} (given by the caller); since = that value.`
        : 'since = last_head (empty if none).')),
    '   If since is non-empty: since_reachable = whether `git cat-file -e <since>^{commit}` succeeds (try `git fetch origin <since>` once if not).',
    '   If reachable: delta_files = the paths from `git diff --name-only <since> HEAD` that are also in files.',
    '6. Prior findings, if last_round > 0 (then prior must be reported, even as []). Shell variables do not survive between commands, so run each',
    '   read exactly as written: each recomputes the write-access list and keeps only its authors. If any exits non-zero or prints an error,',
    '   return ok=false naming step 6.',
    `   set -o pipefail; ${WRITERS} && gh api --paginate 'repos/{owner}/{repo}/pulls/${pr}/comments' | jq -s --argjson w "$W" '[.[][] | ${TRUSTED} | {id, in_reply_to_id, path, line: (.original_line // .line), body}]'`,
    `   set -o pipefail; ${WRITERS} && gh api --paginate 'repos/{owner}/{repo}/issues/${pr}/comments' | jq -s --argjson w "$W" '[.[][] | ${TRUSTED} | {id, body}]'`,
    `   gh api 'repos/{owner}/{repo}/pulls/${pr}/reviews/<review-id>' --jq .body   (only for the review ids step 4 printed)`,
    '   Inline findings are the review comments whose body starts with an ID (**R<round>.<k>**) and whose in_reply_to_id is null; their outcomes are',
    '   the replies whose in_reply_to_id is that comment\'s id. "Outside the diff" findings are listed by ID in the step-4 reviews\' bodies; their',
    '   outcomes are in the PR comments that name the ID.',
    '   For each ID report file, line (as the read above projects it: the line at the head that round reviewed, which is what the finding\'s own text cites), title, outcome (fixed / declined / open when nothing records one / unknown) and a one-line note quoting the recorded reason.',
    '',
    'If any step fails, return ok=false with an error naming the step and what it printed. Never return ok=true without head_sha, merge_base, files and last_round.',
  ].join('\n'),
  { label: 'scope', phase: 'Scope', schema: SCOPE, ...CHORE },
)

if (!scope || !scope.ok) {
  return { pr, error: (scope && scope.error) || 'the scope stage returned nothing' }
}
if (!SHA.test(scope.head_sha || '') || !SHA.test(scope.merge_base || '')) {
  return { pr, error: 'the scope stage reported ok without a full head_sha and merge_base; re-run the round' }
}
if (!Array.isArray(scope.files) || scope.files.length === 0) {
  return { pr, error: `no changed files between ${short(scope.merge_base)} and ${short(scope.head_sha)}: either the PR diff is empty or the scope stage failed to list it` }
}
// A failed read of the previous rounds must never pass as "round 1": that
// would reuse finding IDs, drop every recorded outcome and reset the budget.
if (!Number.isInteger(scope.last_round) || scope.last_round < 0
  || (scope.last_head && !SHA.test(scope.last_head))
  || (scope.last_round > 0 && !Array.isArray(scope.prior))) {
  return { pr, error: 'the scope stage did not report the previous rounds (last_round, last_head, prior); re-run the round' }
}
if (scope.state && scope.state !== 'OPEN') log(`PR #${pr} is ${scope.state}; reviewing it anyway`)

const head = scope.head_sha
const mb = scope.merge_base
const round = Number.isInteger(A.round) ? A.round : scope.last_round + 1
const sinceWanted = A.full ? '' : (scope.since || '')
const files = scope.files.filter(f => !LOCKFILE.test(f))

if (sinceWanted && sinceWanted === head) {
  return { pr, round: scope.last_round, head, skipped: `head ${short(head)} was already reviewed (round ${scope.last_round}); push a change or pass full` }
}

let mode = 'full'
let since = ''
let covered = files
if (sinceWanted && scope.since_reachable === true && Array.isArray(scope.delta_files)) {
  mode = 'delta'
  since = sinceWanted
  covered = scope.delta_files.filter(f => !LOCKFILE.test(f))
} else if (sinceWanted) {
  log(`delta base ${short(sinceWanted)} is not confirmed reachable; reviewing the whole PR`)
}

const prior = scope.prior || []
const priorText = prior.length
  ? prior.map(p => `- ${p.id} [${p.outcome}] ${p.file || '?'}:${p.line || '?'} — ${p.title}${p.note ? ' — ' + p.note : ''}`).join('\n')
  : 'none'

const selected = LENSES
  .map(l => ({ ...l, files: covered.filter(l.applies) }))
  .filter(l => l.files.length > 0)
const lensesRequested = selected.filter(l => skip.includes(l.key)).map(l => l.key)
const lenses = selected.filter(l => !skip.includes(l.key))
log(`round ${round}, ${mode}${since ? ' since ' + short(since) : ''}: ${covered.length} file(s); lenses ${lenses.map(l => l.key).join(', ') || 'none'}${lensesRequested.length ? '; skipped by request: ' + lensesRequested.join(', ') : ''}`)

const lensesRun = []
const lensesFailed = []
const raw = []
let verified = []

// =================================================================== Review

if (lenses.length) {
  phase('Review')

  const lensPrompt = l => [
    `Adversarial review round ${round} of PR #${pr}: "${scope.title}". Your lens: ${l.key}.`,
    '',
    `What the PR is for: ${scope.purpose}`,
    '',
    `The working tree is checked out at the PR head ${head}. Merge-base with ${scope.base_ref || 'the base branch'}: ${mb}.`,
    mode === 'full'
      ? `Review the whole PR: git diff ${mb} ${head}`
      : `This is a delta round. Review what changed since the last reviewed head: git diff ${since} ${head} -- <files below>. Report defects the delta introduced, and prior findings it claims to fix but does not. Read the full PR diff (git diff ${mb} ${head}) for context.`,
    `Files in your lens's scope: ${l.files.join(', ')}`,
    'Skip lockfiles (Cargo.lock, MODULE.bazel.lock).',
    '',
    'Findings from earlier rounds. Do not raise a fixed or declined one again without new evidence that its outcome is wrong, and say so if you do:',
    priorText,
    '',
    l.plugin ? PLUGIN_BRIEF : 'Read docs/skills/adversarial-review.md section "Ground rules" first and follow them. Never modify the checkout.',
    '',
    'Report at most 8 findings, most severe first, and set omitted to how many more you dropped. Every line number is at the PR head. An empty list is the right answer when the code is right.',
  ].join('\n')

  const runLens = async l => {
    const opts = { label: `review:${l.key}`, phase: 'Review', schema: FINDINGS, agentType: l.agentType, ...LENS }
    let r = null
    try { r = await agent(lensPrompt(l), opts) } catch (e) { r = null }
    if (!r) {
      log(`lens ${l.key} returned nothing; retrying once`)
      try { r = await agent(lensPrompt(l), { ...opts, label: `review:${l.key}:retry` }) } catch (e) { r = null }
    }
    return r
  }

  const lensResults = await parallel(lenses.map(l => () => runLens(l)))
  lensResults.forEach((r, i) => {
    const l = lenses[i]
    if (!r) {
      lensesFailed.push(l.key)
      log(`lens ${l.key} failed twice` + (l.plugin ? ` — is pr-review-toolkit installed? ${PLUGIN_INSTALL}` : ''))
      return
    }
    lensesRun.push(l.key)
    if (r.omitted > 0) log(`lens ${l.key} dropped ${r.omitted} lower-ranked finding(s) at the cap`)
    for (const f of r.findings || []) raw.push({ ...f, lenses: [l.key], also_at: f.also_at || [] })
  })

  // ================================================================= Dedupe

  phase('Dedupe')

  // Every merge goes through the same-root-cause question: two findings on
  // one line can be two defects, and merging them unasked would verify only
  // one.
  let findings = raw
  if (findings.length >= 2) {
    const listing = findings.map((f, i) => `${i}. [${f.lenses.join('+')}] ${f.file}:${f.line} — ${f.title} — ${clip(f.consequence, 200)}`).join('\n')
    const g = await agent(
      [
        'These findings came from different reviewers of the same pull request. Group the indices that describe the same underlying defect — same root cause, so one fix resolves all of them.',
        'Do not group findings that are merely nearby, on the same line, in the same file, or in the same category: two findings on one line can be two defects. Omit singletons. Read the code if you need to.',
        '',
        listing,
      ].join('\n'),
      { label: 'dedupe', phase: 'Dedupe', schema: GROUPS, ...CHORE },
    )
    // "Same root cause" is transitive: overlapping groups are one group.
    // Merging them into connected components first means no finding is
    // ever absorbed into one that is itself absorbed later.
    const parent = findings.map((_, i) => i)
    const rootOf = i => (parent[i] === i ? i : (parent[i] = rootOf(parent[i])))
    for (const grp of (g && g.groups) || []) {
      const ok = [...new Set(grp)].filter(i => Number.isInteger(i) && i >= 0 && i < findings.length)
      for (const i of ok.slice(1)) parent[rootOf(i)] = rootOf(ok[0])
    }
    const components = new Map()
    findings.forEach((_, i) => components.set(rootOf(i), [...(components.get(rootOf(i)) || []), i]))
    const groups = [...components.values()].filter(grp => grp.length >= 2)
    const absorbed = new Set()
    for (const grp of groups) {
      const members = grp.map(i => findings[i]).sort((a, b) => SEV_RANK[a.severity] - SEV_RANK[b.severity])
      const keep = members[0]
      for (const m of members.slice(1)) {
        keep.lenses = [...new Set([...keep.lenses, ...m.lenses])]
        keep.also_at = [...new Set([...keep.also_at, `${m.file}:${m.line}`, ...m.also_at])]
        keep.merged = [...(keep.merged || []), m.title]
        keep.members = [...(keep.members || []), { ...m }]
      }
      grp.forEach(i => { if (findings[i] !== keep) absorbed.add(i) })
    }
    if (absorbed.size) log(`dedupe merged ${absorbed.size} duplicate finding(s)`)
    findings = findings.filter((_, i) => !absorbed.has(i))
  }
  log(`${raw.length} raised, ${findings.length} after dedupe`)

  // ================================================================= Verify

  phase('Verify')

  const ANGLES = {
    trace: 'Your angle: trace the trigger. Walk every link from the stated input or state to the stated consequence at the PR head; refute if any link fails.',
    handled: 'Your angle: look for what already handles it at the PR head — an upstream guard, validation at load, a lock, a test that pins the behaviour, a later commit in the PR — and decide whether the PR introduced or touched it, or it is pre-existing.',
    claims: 'Your angle: check every factual claim. The cited code says what is claimed; any external behaviour is documented; nothing predicts a build or lint outcome; the suggested remedy would actually fix it.',
  }

  const verifyPrompt = (f, angle) => [
    `Verify one finding from adversarial review round ${round} of PR #${pr}: "${scope.title}".`,
    `What the PR is for: ${scope.purpose}`,
    `The working tree is at the PR head ${head}; the PR diff is git diff ${mb} ${head}.`,
    '',
    'Finding:',
    JSON.stringify({ lens: f.lenses.join('+'), title: f.title, file: f.file, line: f.line, severity: f.severity, trigger: f.trigger, consequence: f.consequence, evidence: f.evidence, remedy: f.remedy || '', also_at: f.also_at }, null, 2),
    f.merged && f.merged.length ? `Other reviewers stated the same defect as: ${f.merged.join('; ')}.` : '',
    'Judge the defect. If it is real but this statement overstates it — its severity, trigger or consequence — vote on the defect and say what is overstated in statement_note.',
    '',
    angle ? ANGLES[angle] : 'Apply every angle: ' + Object.values(ANGLES).join(' '),
    'Earlier rounds\' findings, for spotting a duplicate of a fixed or declined one:',
    priorText,
  ].join('\n')

  const verifyStatement = async f => {
    const angles = f.severity === 'high' ? ['trace', 'handled', 'claims'] : [null]
    const votes = (await parallel(angles.map(a => () => agent(verifyPrompt(f, a), {
      label: `verify:${f.lenses[0]}:${String(f.file).split('/').pop()}:${f.line}${a ? ':' + a : ''}`,
      phase: 'Verify',
      schema: VERDICT,
      agentType: 'review-verifier',
      ...VERIFY,
    })))).filter(Boolean)
    const expected = angles.length
    const need = f.severity === 'high' ? 2 : 1
    const alive = votes.filter(v => v.verdict !== 'refuted')
    let status
    if (alive.length >= need) {
      const pre = alive.filter(v => v.verdict === 'pre_existing').length
      status = pre > alive.length - pre ? 'pre_existing' : 'confirmed'
    } else if (alive.length + (expected - votes.length) >= need) {
      status = 'unverified' // skeptics died; never drop a finding silently
    } else {
      status = 'refuted'
    }
    const notes = votes.map(v => v.remedy_note).filter(Boolean)
    const reasons = votes.filter(v => v.verdict === 'refuted').map(v => v.reasoning)
    const overstated = alive.map(v => v.statement_note).filter(Boolean)
    const tally = {
      confirmed: votes.filter(v => v.verdict === 'confirmed').length,
      pre_existing: votes.filter(v => v.verdict === 'pre_existing').length,
      refuted: reasons.length,
    }
    return { ...f, status, remedy_note: notes.join(' '), statement_note: overstated.join(' '), refuted_because: reasons[0] || '', votes: votes.length, expected, tally }
  }

  // How well a statement came through its skeptics, best last: confirmed as
  // stated, confirmed but overstated, unverified (skeptics died), judged
  // pre-existing, refuted.
  const rankOf = v => (v.status === 'confirmed' ? (v.statement_note ? 3 : 4)
    : v.status === 'unverified' ? 2 : v.status === 'pre_existing' ? 1 : 0)

  // Dedupe kept the most severe statement of a group, which is also the
  // likeliest to overclaim. Unless its skeptics confirmed it as stated,
  // each absorbed statement gets its own skeptics at its own severity, and
  // the best-ranked statement is posted. Only a strictly better rank
  // replaces the current best, so the outcome does not depend on the order
  // the members are tried in. A pre-existing verdict on the kept statement
  // is a judgement on the defect, not the wording, so it stands. Any other
  // statement of the defect whose skeptics did not all return is carried
  // on the posted finding: a more severe claim nobody could check must not
  // vanish behind a milder one that was checked.
  const verifyOne = async f => {
    const v = await verifyStatement(f)
    if (!(f.members && f.members.length) || v.status === 'pre_existing' || rankOf(v) === 4) return v
    const tried = [v]
    let best = v
    let bestFrom = v
    for (const m of f.members) {
      const mv = await verifyStatement(m)
      tried.push(mv)
      if (rankOf(mv) <= rankOf(best)) continue
      const others = [f, ...f.members.filter(x => x !== m)]
      best = {
        ...mv,
        lenses: f.lenses,
        also_at: [...new Set(others.map(x => `${x.file}:${x.line}`).concat(f.also_at))].filter(x => x !== `${mv.file}:${mv.line}`),
        merged: others.map(x => x.title),
      }
      bestFrom = mv
      if (rankOf(best) === 4) break
    }
    const unchecked = tried
      .filter(t => t !== bestFrom && t.status === 'unverified')
      .map(t => ({ severity: t.severity, title: t.title, consequence: t.consequence, votes: t.votes, expected: t.expected, tally: t.tally, refuted_because: t.refuted_because, statement_note: t.statement_note }))
    if (unchecked.length) {
      log(`${unchecked.length} statement(s) of "${clip(best.title, 80)}" could not be verified; recorded on the posted finding`)
      best = { ...best, unchecked }
    }
    return best
  }

  verified = (await parallel(findings.map(f => () => verifyOne(f)))).filter(Boolean)
  if (verified.length < findings.length) log(`${findings.length - verified.length} finding(s) lost in verification; re-run the round`)
}

const bySeverity = (a, b) => SEV_RANK[a.severity] - SEV_RANK[b.severity]
const posted = verified.filter(f => f.status === 'confirmed' || f.status === 'unverified').sort(bySeverity)
const preExisting = verified.filter(f => f.status === 'pre_existing').sort(bySeverity)
const refuted = verified.filter(f => f.status === 'refuted')
let k = 0
for (const f of posted) f.id = `R${round}.${++k}`
for (const f of preExisting) f.id = `R${round}.${++k}`
if (lenses.length) log(`verified: ${posted.length} confirmed/unverified, ${preExisting.length} pre-existing, ${refuted.length} refuted`)

// =================================================================== Settle

// The lenses read the shared working tree, so the round only describes the
// head if the checkout stayed on it, and is only worth posting if the PR
// did not move on while it ran.
phase('Settle')

const st = await agent(
  [
    `You are the settle stage of adversarial review round ${round} of PR #${pr}, which reviewed head ${head}. Run exactly these commands and report what they print. Do not review anything, and never modify the checkout.`,
    '1. Run this as ONE command, in this order — status first, so an edit committed mid-check still shows as a moved HEAD:',
    '   git status --porcelain --untracked-files=no; echo ---; git rev-parse HEAD',
    '   checkout_dirty = whether anything printed above the --- line; checkout_head = the line below it.',
    `2. Then: gh pr view ${pr} --json headRefOid --jq .headRefOid → pr_head.`,
    posted.length
      ? [
        '3. For each finding below, decide whether GitHub accepts an inline comment on its line. For each distinct file run',
        `   gh api --paginate 'repos/{owner}/{repo}/pulls/${pr}/files' | jq -s -r --arg f '<file>' '.[][] | select(.filename == $f) | .patch // ""' | awk '/^@@/{split($3,a,","); s=substr(a[1],2); n=(a[2]==""?1:a[2]); if (n>0) print s, s+n-1}'`,
        '   Each output line is an inclusive range "start end" of commentable lines. in_diff is true only if the line lies inside one. A file with no output (absent from the PR, a pure rename, binary or too large) has no commentable lines.',
        '',
        posted.map((f, i) => `${i}. ${f.file}:${f.line}`).join('\n'),
      ].join('\n')
      : '3. There are no findings to anchor: anchors = [].',
  ].join('\n'),
  { label: 'settle', phase: 'Settle', schema: SETTLE, ...CHORE },
)

const incomplete = lensesFailed.map(key => `${key} returned nothing` + (key === 'silent-failures' ? ` (is the plugin installed? ${tick(PLUGIN_INSTALL)})` : ''))
if (!st || !SHA.test(st.checkout_head || '') || !SHA.test(st.pr_head || '')) {
  incomplete.push('the settle stage could not confirm the checkout and PR head')
} else {
  if (st.pr_head !== head) {
    return { pr, round, head, superseded: `the PR head moved to ${short(st.pr_head)} while the round ran: discard this round unposted and run one on the new head` }
  }
  if (st.checkout_head !== head || st.checkout_dirty) {
    incomplete.push(`the checkout changed while the round ran (HEAD ${short(st.checkout_head)}${st.checkout_dirty ? ', uncommitted changes' : ''}), so its reads may not be of the head`)
  }
  for (const an of st.anchors || []) {
    if (Number.isInteger(an.index) && posted[an.index]) posted[an.index].inline = an.in_diff === true
  }
}
const complete = incomplete.length === 0

// ------------------------------------------------------------ review payload

const marker = `<!-- adversarial-review round=${round}${complete ? ' head=' + head : ''} -->`

// An unverified statement is only readable with its verdicts: one returned
// refutation and one returned confirmation are opposite answers.
const voteSplit = t => `${t.votes} of ${t.expected} skeptics returned: ${t.tally.confirmed} confirmed, ${t.tally.pre_existing} pre-existing, ${t.tally.refuted} refuted`

const findingBody = f => [
  `**${f.id}** · ${f.lenses.join(' + ')} · ${f.severity}` + (f.status === 'unverified' ? ` · **unverified** (${voteSplit(f)})` : ''),
  '',
  `**${f.title}**`,
  '',
  `**Trigger:** ${f.trigger}`,
  `**Consequence:** ${f.consequence}`,
  `**Evidence:** ${f.evidence}`,
  f.also_at.length ? `**Also at:** ${f.also_at.join(', ')}` : null,
  f.merged && f.merged.length ? `**Also raised as:** ${f.merged.map(t => clip(t, 120)).join('; ')}` : null,
  f.remedy ? `**Suggested remedy:** ${f.remedy}` : null,
  f.statement_note ? `**Skeptic on the statement:** ${f.statement_note}` : null,
  f.status === 'unverified' && f.refuted_because ? `**Skeptic who refuted it:** ${clip(f.refuted_because, 400)}` : null,
  ...(f.unchecked || []).map(u => `**Unverified statement:** ${u.severity} "${clip(u.title, 160)}" — ${voteSplit(u)} — ${clip(u.consequence, 240)}`
    + (u.refuted_because ? ` — refuted because: ${clip(u.refuted_because, 240)}` : '')
    + (u.statement_note ? ` — overstated: ${clip(u.statement_note, 160)}` : '')),
  f.remedy_note ? `**Skeptic on the remedy:** ${f.remedy_note}` : null,
].filter(x => x !== null).join('\n')

const unverifiedCount = posted.filter(f => f.status === 'unverified').length
  + posted.reduce((n, f) => n + (f.unchecked ? f.unchecked.length : 0), 0)
const alsoStated = f => (f.merged && f.merged.length ? ` (also stated as: ${f.merged.map(t => clip(t, 80)).join('; ')})` : '')
const inline = posted.filter(f => f.inline)
const outside = posted.filter(f => !f.inline)

const body = [
  `### Adversarial review — round ${round}`,
  '',
  `Head ${tick(short(head))} · ` + (mode === 'full' ? 'full review of the PR' : `delta since ${tick(short(since))}`)
    + ` · lenses: ${lensesRun.join(', ') || 'none applied'}`
    + (lensesRequested.length ? ` · skipped by request: ${lensesRequested.join(', ')}` : ''),
  `Lenses: ${MODEL} at ${LENS_EFFORT} effort · skeptics: ${MODEL} at ${VERIFY_EFFORT} · scope, dedupe, settle: ${MODEL} at ${CHORE_EFFORT}`,
  incomplete.length ? `\n**Incomplete round** — this head does not count as reviewed: ${incomplete.join('; ')}.` : null,
  lenses.length ? null : '\nNo file in this round is covered by a lens.',
  '',
  `Raised ${raw.length} → ${posted.length} confirmed` + (unverifiedCount ? ` (${unverifiedCount} unverified statement(s) included)` : '') + `, ${preExisting.length} pre-existing, ${refuted.length} refuted.`
    + (posted.length ? ` ${inline.length} inline, ${outside.length} below.` : ''),
  posted.length ? null : '\n**No confirmed findings.**',
  outside.length ? '\n#### Outside the diff\n\nRecord each outcome in a PR comment that names its ID.' : null,
  ...outside.map(f => '\n' + findingBody(f) + `\n\n*At* ${tick(f.file + ':' + f.line)}`),
  preExisting.length
    ? `\n<details><summary>Pre-existing, not introduced by this PR (${preExisting.length}) — follow-up candidates, not findings</summary>\n\n`
      + preExisting.map(f => `- **${f.id}** [${f.lenses.join('+')}] ${tick(f.file + ':' + f.line)} — ${clip(f.title, 160)}${alsoStated(f)} — ${clip(f.consequence, 240)}`).join('\n')
      + '\n</details>'
    : null,
  refuted.length
    ? `\n<details><summary>Refuted by the skeptics (${refuted.length}) — not findings; listed so the verifier can be audited</summary>\n\n`
      + refuted.map(f => `- [${f.lenses.join('+')}] ${tick(f.file + ':' + f.line)} — ${clip(f.title, 160)}${alsoStated(f)} — *${clip(f.refuted_because, 300)}*`).join('\n')
      + '\n</details>'
    : null,
  '',
  marker,
].filter(x => x !== null).join('\n')

return {
  pr,
  round,
  head,
  mode,
  since,
  complete,
  quiet: complete && posted.length === 0,
  incomplete,
  models: { lens: LENS, verify: VERIFY, chore: CHORE },
  lenses_run: lensesRun,
  lenses_failed: lensesFailed,
  lenses_skipped: lensesRequested,
  findings: posted.map(f => ({ id: f.id, status: f.status, severity: f.severity, lenses: f.lenses, file: f.file, line: f.line, title: f.title, inline: !!f.inline })),
  pre_existing: preExisting.map(f => ({ id: f.id, file: f.file, line: f.line, title: f.title })),
  refuted: refuted.map(f => ({ lenses: f.lenses, file: f.file, line: f.line, title: f.title, also_stated_as: f.merged || [], because: f.refuted_because })),
  review: {
    commit_id: head,
    event: 'COMMENT',
    body,
    comments: inline.map(f => ({ path: f.file, line: f.line, side: 'RIGHT', body: findingBody(f) })),
  },
}
