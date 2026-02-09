OSS / SOUP Management
=====================

Open Source Software components require qualification evidence per
ISO 26262 Part 8 Clause 12 and IEC 62304 SOUP management requirements.

OSS Classification
------------------

.. list-table::
   :header-rows: 1

   * - Component
     - Role
     - License
     - Classification
     - Risk
   * - **loom**
     - Optimizer
     - Apache-2.0
     - OSS tool (qualified)
     - TI2: transforms code semantics
   * - **meld**
     - Static fuser
     - Apache-2.0
     - OSS tool (qualified)
     - TI2: transforms component structure
   * - **wasmparser**
     - WASM decoder
     - Apache-2.0
     - SOUP
     - TI2: parsing errors propagate
   * - **z3**
     - SMT solver
     - MIT
     - SOUP (verification)
     - TI1: verification tool, cannot introduce errors
   * - **git2/libgit2**
     - Version control
     - MIT/GPL2
     - SOUP (tooling)
     - TI1: configuration management, not in data path

SOUP Management per IEC 62304
------------------------------

For each SOUP item, we track:

1. **Identification**: name, version, manufacturer, unique ID
2. **Requirements**: functional/performance requirements for safety use
3. **Known anomalies**: CVEs, bugs affecting our use case
4. **Risk assessment**: impact if the SOUP item fails
5. **Risk controls**: how we mitigate SOUP failures

OSS Qualification Strategy
---------------------------

For loom and meld as open-source tools:

**Development Process Evidence**:

- CI/CD pipeline (GitHub Actions)
- Test suites (cargo test)
- Formal verification (Z3, Rocq)
- Code review (pull request process)
- Release management (tagged releases, changelogs)
- Issue tracking (GitHub Issues)

**Verification Evidence**:

- Proof build logs (Rocq/Z3 output)
- Test reports (cargo test output)
- Integration test results (pipeline end-to-end)
- Dogfooding results (loom optimizes itself)

**Configuration Management**:

- Git version control
- Pinned dependencies (Cargo.lock)
- Reproducible builds (Bazel hermetic builds)
- Signed releases (git tag --sign)
