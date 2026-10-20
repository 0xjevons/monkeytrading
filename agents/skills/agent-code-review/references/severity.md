# Agent code review: severity guide

Load for every [agent-code-review](../SKILL.md) run, and hand its absolute path to every
multi-agent reviewer. Grade each finding by its impact; confidence is a separate field.

- **Critical** — loss of funds, a security breach, data corruption, or an active bypass of a
  safety or process rule.
- **High** — wrong behavior on a realistic path, a silent failure, a lost or duplicated effect
  after a restart, an unsafe rollout, or a violation of a safety or process rule.
- **Medium** — wrong behavior on an edge path, an untested changed branch, a test that does not
  test what it claims, or a failure path nobody would notice.
- **Low** — maintainability or consistency cost with no behavior impact.
- **Nit** — style or wording.

Safety and process rules are those whose breach does harm by itself: `no-deploy`, secrets, pushes
or GitHub posts without approval, and `no-guessing-current-state`. Any other rule or skill — `use`
statements, naming, comments, formatting, `minimal-change` — is graded by its impact like any other
finding, usually Low or Nit.
