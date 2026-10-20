---
name: ask-agent
description: Run a fresh or resumed Claude or Codex headless session at xhigh effort, persist its response, and record its session ID for a later follow-up.
---

Load on request when a task needs an independent Claude or Codex agent, or a
follow-up to a known agent session. Callers select a provider, output path, and
prompt; the model defaults below apply unless the caller explicitly overrides
them.

## Models

All calls use `xhigh` effort. This section is the single source of model IDs; other skills
reference it instead of restating them.

### Supported models

- **Claude:** `claude-sonnet-5`, `claude-sonnet-5-5`, `claude-opus-5`, `claude-opus-5-5`, and
  `claude-fable-5-1`.
- **Codex:** `gpt-5.5`, `gpt-5.6-luna`, `gpt-5.6-terra`, `gpt-5.6-sol`, `gpt-6-luna`,
  `gpt-6-sol`, `gpt-6.1-sol`, and `gpt-6-astra`.

Use the [OpenAI model catalog](https://developers.openai.com/api/docs/models) as the
Codex model-ID source. CLI release and account access determine availability; validate an ID
with a fresh session before dispatching it.

### Review-level roster

Review levels give review skills a consistent roster:

| Review level | Claude | Codex |
| --- | --- | --- |
| `low` | `claude-sonnet-5-5` | `gpt-6-luna` |
| `medium` (default) | `claude-opus-5-5` | `gpt-6.1-sol` |
| `high` | `claude-fable-5-1` | `gpt-6-astra` |

`claude-sonnet-5-5` requires Claude Code 2.1.284 or newer, and `claude-opus-5-5` 2.1.280 or newer.

`claude-fable-5-1` requires Claude Code 2.1.251 or newer. Claude Code can log an
`unrecognized_model` diagnostic while passing `claude-fable-5-1` to the API; use the
response JSON and command exit status to determine whether a run succeeded.

`gpt-6-astra` requires Codex CLI 0.153.4 or newer, `gpt-6-sol` and `gpt-6-luna` 0.156.1 or
newer, and `gpt-6.1-sol` 0.159.1 or newer: the releases that added them to its model catalog.
Every GPT-6 model (`gpt-6-*`, `gpt-6.1-*`) also needs an account with access to it.

Judge a Codex run the same way: by exit status and output. A "Model metadata … not found" warning
means the CLI predates the model and the run may be degraded; update the CLI. A 400
`invalid_request_error` saying the model is not supported for the account is an entitlement
failure that no CLI update fixes.

When a roster model is unavailable, stop and report it; run another model only when the user
names it.

For a single call, use the `medium` model for its provider unless the caller
names another model. A review launches only the Claude and Codex pair listed
for every requested level.

## Fresh session

Use one fresh session for each independent task. Put only the task's required
scope in the prompt; do not pass the caller's conversation or another agent's
findings.

### Claude

```bash
prompt_file='<prompt-file>'
agent_prompt="$(< "$prompt_file")"
claude -p --model <model> --effort xhigh --output-format json "$agent_prompt" \
  < /dev/null > <response-json> 2> <stderr>
test $? -eq 0 || exit 1
jq -r -e '.result | select(type == "string" and length > 0)' <response-json> \
  > <output-file> || exit 1
test -s <output-file> || exit 1
jq -r -e '.session_id | select(type == "string" and length > 0)' <response-json> \
  > <session-id-file> || exit 1
test -s <session-id-file> || exit 1
```

### Codex

```bash
prompt_file='<prompt-file>'
agent_prompt="$(< "$prompt_file")"
codex exec -m <model> -c 'model_reasoning_effort="xhigh"' \
  --skip-git-repo-check --json -o <output-file> "$agent_prompt" \
  < /dev/null > <events-jsonl> 2> <stderr>
test $? -eq 0 || exit 1
test -s <output-file> || exit 1
jq -r -e 'select(.type == "thread.started") | .thread_id' <events-jsonl> \
  > <session-id-file> || exit 1
test -s <session-id-file> || exit 1
```

`<output-file>` is the agent's final message. Keep the provider-specific JSON
or JSONL only as run bookkeeping; the caller reads the Markdown output file.

## Continue a session

Resume only the exact ID saved by the fresh call. Never use a most-recent or
implicit-session option: another concurrent agent could select the wrong run.
Keep the same model and effort as the fresh call.

### Claude

```bash
prompt_file='<prompt-file>'
agent_prompt="$(< "$prompt_file")"
claude -p --resume "$(cat <session-id-file>)" --model <model> --effort xhigh \
  --output-format json "$agent_prompt" \
  < /dev/null > <response-json> 2> <stderr>
test $? -eq 0 || exit 1
jq -r -e '.result | select(type == "string" and length > 0)' <response-json> \
  > <output-file> || exit 1
test -s <output-file> || exit 1
```

### Codex

```bash
prompt_file='<prompt-file>'
agent_prompt="$(< "$prompt_file")"
codex exec resume -m <model> -c 'model_reasoning_effort="xhigh"' \
  --skip-git-repo-check --json -o <output-file> "$(cat <session-id-file>)" \
  "$agent_prompt" < /dev/null > <events-jsonl> 2> <stderr>
test $? -eq 0 || exit 1
test -s <output-file> || exit 1
```

## Rules

- Redirect stdin from `/dev/null` whenever the prompt is an argument. This
  prevents a headless session from waiting for additional input.
- Save the prompt in `<prompt-file>` and pass the loaded variable as one quoted
  argument. Never interpolate prompt text into shell source.
- Store each agent's output, session ID, stderr, and structured output under
  the caller's gitignored run directory.
- Treat a failed command, unparsable response, or empty output or session ID as
  an incomplete run. Do not resume, cross-validate, or consolidate it.
- A resumed session is appropriate for a follow-up that needs its own prior
  reasoning. Start a new session when independence matters.
- Do not use sandbox-bypass flags. The caller's prompt must say which files it
  may read or edit and what it must return.
