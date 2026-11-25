---
name: init
description: Write or update the AGENTS.md of a project. Use when asked to set up or document a repository for the agent.
---
Write an `AGENTS.md` at the root of the repository: the instructions an agent needs to work here.
- Read first: the README, the build files (`Cargo.toml`, `package.json`, `Makefile`, `pyproject.toml` and the like), the CI configuration and the layout of the source tree.
- Write only what you verified in the files. Don't guess a command: find it in the build files or the CI.
- Cover how to build, test, lint and format (with the command for a single test), where the code lives, and the conventions the code visibly follows.
- Leave out what any agent already knows, and what the README says well: link to it.
- Keep it short, about 20 to 40 lines of plain bullet points.
- When `AGENTS.md` exists, keep what is there. Fix what is out of date and add what is missing, and say what you changed.
- Ask the user about what the files cannot tell you, such as rules for commits or reviews, before you write it.
