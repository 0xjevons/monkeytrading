# Atomic sessions

Load on request when a task spans more than one focused session, or when the
conversation history is past the point where it helps.

**Rule**: Split long work into small sessions with a reviewed handoff note.
Don't drag the full transcript forever.

- Treat failed or likely remote compaction as a session boundary. Before context
  gets low, checkpoint work to a repo file or handoff note so the next session
  can resume without the full chat transcript.
- Save a concise handoff file with: goal, decisions made, attempts that failed,
  open questions, and the next concrete check. Include source links and
  unresolved questions explicitly — bare summaries hide what matters.
- Start the next session from that file only, not from the prior chat history.
- Once the work merges, route durable knowledge to memory or repo docs per
  [memory.md](memory.md) and delete the
  handoff; stale session notes are litter.

Why: old exploration context pollutes execution and exhausts the window.
Atomic sessions keep each step's context tight and let the next agent (or the
next you) pick up from facts, not log noise.
