Automotive SPICE Compliance
===========================

The automator pipeline maps to Automotive SPICE (ASPICE) process areas,
enabling process assessment at capability levels 1-3.

SWE Process Mapping
-------------------

.. list-table::
   :header-rows: 1
   :widths: 15 25 30 30

   * - Process
     - Activity
     - Automator Stage
     - Work Products
   * - SWE.1
     - SW Requirements Analysis
     - Planner agent reads ROADMAP.md, issues, consistency report
     - ``req`` needs, task descriptions
   * - SWE.2
     - SW Architectural Design
     - Planner architecture decisions, cross-repo design
     - ``arch`` needs, design rationale
   * - SWE.3
     - SW Detailed Design & Unit Construction
     - Implementer agent writes code + tests
     - ``impl`` needs, git branches, code
   * - SWE.4
     - SW Unit Verification
     - Gate 1 (fmt, clippy, test) + Gate 2 (Z3, Rocq)
     - ``utest`` + ``proof`` needs, gate reports
   * - SWE.5
     - SW Integration & Integration Test
     - Gate 3 (meld → loom → synth pipeline)
     - ``itest`` needs, pipeline results
   * - SWE.6
     - SW Qualification Test
     - Release pipeline, dogfooding, certification tests
     - ``verif`` + ``rel`` needs, attestations

Supporting Processes
--------------------

.. list-table::
   :header-rows: 1

   * - Process
     - Automator Coverage
   * - SUP.8 Configuration Management
     - git2 operations, branch naming convention, Cargo.lock pinning
   * - SUP.9 Problem Resolution
     - Task rejection with feedback, re-implementation cycle
   * - SUP.10 Change Request Management
     - Task queue, approval workflow, consistency checker
   * - MAN.3 Project Management
     - Budget tracking, status dashboard, task prioritization

Bidirectional Traceability
--------------------------

ASPICE requires bidirectional traceability at capability level 2+:

.. list-table::
   :header-rows: 1

   * - From
     - To
     - sphinx-needs Link
   * - SW Requirements (SWE.1)
     - SW Architecture (SWE.2)
     - ``req`` → ``arch`` via ``links``
   * - SW Architecture (SWE.2)
     - SW Units (SWE.3)
     - ``arch`` → ``impl`` via ``links``
   * - SW Units (SWE.3)
     - Unit Verification (SWE.4)
     - ``impl`` → ``utest``, ``proof`` via ``links``
   * - SW Requirements (SWE.1)
     - Integration Tests (SWE.5)
     - ``req`` → ``itest`` via ``links``
   * - SW Requirements (SWE.1)
     - Qualification Tests (SWE.6)
     - ``req`` → ``verif`` via ``links``

The ``needflow`` and ``needtable`` directives in Sphinx render these
relationships as visual graphs and audit-ready matrices.

Process Capability Assessment
-----------------------------

The automator enables assessment at:

- **Level 1 (Performed)**: All SWE processes produce their base practices
  and work products via the autonomous pipeline.

- **Level 2 (Managed)**: Work products are planned (planner agent),
  tracked (task status), reviewed (reviewer agent + human checkpoint),
  and configuration-managed (git).

- **Level 3 (Established)**: Standardized process via consistent agent
  prompts, gate definitions, and cross-repo consistency checks.
