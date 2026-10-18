# Sulla

The point of this program is to interact with agents that don't make me want to kill myself.
I want a focus on creating several agents, but being able to dictate and murder them as I please.
I want to be bale to dispatch tasks via beads, and have the orchestrator pick them up. Then view
diffs, and decide if I want to merge in one view.

## Views

There are a few important views that we want.

### Agent progress

The agent progress view will show me current outputs, with ability to input new commands, or stop
the agent in progress.

### Review

When an agent is done I want to be able to view the diff of the change.

### Status Board

Agents should move through a kanban board with their status. Status are as follows:

- Pending -- Not currently running, waiting for resources
- In progress -- Currently itearating and building
- Ready for Review -- Ready to take a peep
- Done -- Should stay here for a few days

From this board we should be able to select an agent and look at either the agent progress or
review view.

### Scheduling

Every time a new bead is added the scheduler should start a new agent if resources are available.
Beads should be used as the main interaction point for everything. The agents should move between
available states. They should be scheduled in order of priority.

## Guts

### SCM

We should be using git, specifically work trees to split everything up.

### Caching

For all compiled languages use Kache for caching so that we do not spend a ton of time rebuilding.

### Langauge

Build with Rust and a TUI.

### Control

Keyboard first. Use vim motions
