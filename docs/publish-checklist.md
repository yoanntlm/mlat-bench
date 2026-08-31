# Before this repo goes public

Sequence decided 2026-09-01; run it as one arc together with the
introduction post.

1. File the upstream NaN issue (docs/upstream-issue-draft.md), then delete
   the draft from this repo. The maintainer hears about the bug from us
   before any public README documents it.
2. Decide the mlatd coupling: publish bench and server together, or move
   the four crates to the mlatd repo first and add a git dependency here.
   A half-public state (public bench containing the private server's
   source) is confusing.
3. Sweep, done 2026-09-01, re-verify before flipping:
   - No real receiver coordinates anywhere, history included (verified:
     scenarios use invented city-center points; LocaRDS is public data;
     runs/ was never committed; only .env.example ever added).
   - candidate/TRIAL.md is generic; internal wiring lives in the
     monorepo (netops/MLAT_TRIAL.md).
   - `fuzz` exists for recordings; capture-format.md privacy section is
     current.
4. Read README.md and docs/ once more as a stranger.
5. Flip visibility, then post the introduction.
