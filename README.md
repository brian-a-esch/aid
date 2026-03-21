# aint (AI Note & Tasks)

aint is an agent management tool responsible for managing content of multiple
agents running different tasks. There are many components that we need to build
out. Here are the subsystems of the project needs to implement

## Agent Runner

This must be a _library_ we build. By being a library, we can make it
customizable & configurable. We need to be able to customize prompts, tools,
agent "modes" (like giving it different tasks build/plan/debug) tools This has
a fringe benefit so we can reuse it in different frontend environments.

