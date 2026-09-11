## Memory
- Key: native-harness-release-branch
- Written-by: claude
- Written: 1789116662
- Verified: 1789116662
- Source: explicit

Roadmap #469 (native harness) integrates on branch release/native-harness (draft PR #493 into main, opened 2026-09-11). Every Nxx step lands as its own PR INTO that branch (N01 = PR #497, worktree .claude/worktrees/wt470); the release PR merges into main only when all 23 steps plus #353/#352/#455/#467/#468 are done. ZCHK-VERSION-BUMP merge-bases against origin/main, not the PR base, so the release branch carries 4.0.0 from N01 on; later steps bump only if main overtakes. Native runtime contracts
[truncated]
