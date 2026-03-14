# Design Document Style Guide

This guide defines the style and content expectations for per-package design documents in Hardy.

## Purpose

Design documents capture **architectural decisions and rationale** - the "why" behind the code. They are for engineers who need to understand the system conceptually before diving into implementation details.

## What Belongs in Design Docs

- **Design goals and constraints** that shaped the implementation
- **Key architectural decisions** and the alternatives considered
- **Conceptual models** - how components interact, data flows, trust boundaries
- **Trade-offs** - what was sacrificed for what benefit
- **Integration points** - how this package relates to others in the system
- **RFC/specification interpretation** - how standards requirements influenced design

## What Does NOT Belong in Design Docs

- **API reference material** - struct fields, enum variants, method signatures (these belong in rustdoc)
- **Usage examples** - code snippets showing how to call APIs (rustdoc)
- **Exhaustive lists** - every error type, every flag bit, every field (rustdoc)
- **Implementation details** that don't reflect architectural choices

## Document Structure

```markdown
# <package-name> Design

<One-line description of purpose>

## Design Goals

What properties/qualities was this package designed to achieve?
Why does this package exist rather than using an off-the-shelf solution?

## Architecture Overview

High-level conceptual model. How do the pieces fit together?
Diagrams welcome if they clarify relationships.

## Key Design Decisions

### <Decision 1 Title>

What was decided, why, and what alternatives were rejected.

### <Decision 2 Title>

...

## Integration

How does this package interact with other Hardy components?
What are the contracts/interfaces?

## Standards Compliance

Which RFCs/specifications does this implement?
Any notable interpretation decisions?

## Testing

Link to test plans (don't duplicate their content).
```

## Research and Verification

When writing or updating design documents:

- **Consult the RFC references** in `/workspace/references/` to ensure accurate terminology and correct descriptions of standards compliance. Use the precise language from specifications when describing protocol behaviour, message formats, or compliance requirements.
- **Ask clarifying questions** if the design intent is unclear from examining the code. It is better to ask the maintainer for clarification than to guess or document assumptions that may be incorrect.

## Writing Style

- **Be concise but not terse** - brevity is good, but not at the expense of readability. Write in complete sentences with natural flow.
- **Explain the "why"** - decisions without rationale are not useful
- **Use concrete examples** when they clarify a concept, but not as API documentation
- **Link to RFCs** with section numbers for traceability
- **Bullets are fine when appropriate** - use bullets for lists of items, options, or requirements. But each bullet should be a complete thought, not a terse fragment. Design rationale and explanations typically read better as paragraphs.

## Assumed Reader Background

- **Familiar with DTN/BPv7 concepts** - bundle protocol, EIDs, CLAs, BPSec, etc.
- **May have limited Rust experience** - explain Rust-specific idioms (traits, lifetimes, closures) when they're central to a design decision
- **Likely background in C, C++, or Java** - comparisons to patterns in those languages can help clarify

When Rust concepts are integral to the design (e.g., "we use closures for structural integrity"), briefly explain what the concept achieves rather than assuming the reader understands Rust idioms.

## Questions to Ask When Writing

1. If I deleted this paragraph, would someone misunderstand the design?
2. Is this explaining a decision, or just describing what the code does?
3. Could this information be found by reading the rustdoc or source?
4. Would a new team member understand *why* things are this way?

## Test Plan References

Each design doc should link to relevant test plans but not duplicate their content:

```markdown
## Testing

- [Unit Test Plan](unit_test_plan.md) - brief description of scope
- [Fuzz Test Plan](fuzz_test_plan.md) - brief description of scope
```
