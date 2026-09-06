# Working through Praxec

For substantial tasks in this repository, use Praxec to delegate suitable bounded
work to other models when it improves the outcome. Quality is the first priority,
then completion time and total cost. Follow
[the orchestration skill](docs/skills/praxec-orchestrate/SKILL.md) for discovery,
model selection, independent acceptance, and evidence-based workflow improvements.

Use existing operator model bindings. Keep Codex responsible for integration and
verification. Improve task-local workflows as evidence warrants; preserve the
prior version and results. Do not equate model-reported completion with acceptance
or mock workflow validation with live quality. Keep external actions within the
user's task authorization.

## WSL disk budget

Check both `df -h /` and `df -h /mnt/c` before substantial builds. The Linux
filesystem's virtual capacity is not the Windows host's available storage;
on 2026-09-06 the host had approximately 52 GB free. Use current host free space
as the growth constraint. Prefer targeted builds with debug symbols and
incremental compilation disabled when practical, and reuse existing dependencies.

Preserve active Autopilot-beta development and `../simuli` (Preveti), including
their build artifacts. Limit cleanup to verified disposable artifacts; preserve
repositories, uncommitted changes, evaluation evidence, and shared caches used by
active development. Do not shut down WSL or compact its virtual disk during active
development. Deleting Linux files may not immediately return space to Windows.
