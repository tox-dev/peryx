---
default: change
---

# Write policy decisions from a background batch writer

A project page no longer blocks runtime workers on one metadata write transaction per listed file. A single writer
thread now commits queued policy decisions together, in arrival order, and reads of decisions wait for earlier records.
The writer opens each table once per batch instead of once per decision, and it prunes history down to the retention
bound with one range deletion instead of one removal per row.
