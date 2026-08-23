## Memory
- Key: repo-skills-may-only-add
- Written-by: setup-migration
- Written: 1787496000
- Verified: 1787496000
- Source: setup
- Tags: workflow, skills, trust
- Paths: src/commands/workflow/skill.rs, src/commands/workflow/engine.rs

Repository skills are untrusted requests and can never widen operator policy or capabilities. A repo manifest may only ADD an id; an id colliding with a built-in or an operator-global skill is ignored with a warning (operator-global may still override a built-in). Workflow state is zirv-owned and durable -- only the current step's selected skill context is injected into a prompt, never the whole history.
