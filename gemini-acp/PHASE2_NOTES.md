Phase 2 — terminal/shell migration

The shell builtin remains integrated through Tool/ToolRegistry. The implementation now uses explicit shell analysis, bounded execution, kill-on-drop process cleanup, and bounded output. No parallel tool architecture is introduced.
