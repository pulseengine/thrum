;; Minimal WebAssembly text format module for pipeline integration testing.
;; This module exercises basic arithmetic that all three tools should handle:
;;   meld: fuse (identity for single module)
;;   loom: optimize (constant folding, dead code elimination)
;;   synth: compile to ARM (i32 operations → ARM instructions)

(module
  ;; Simple addition function
  (func $add (export "add") (param i32 i32) (result i32)
    local.get 0
    local.get 1
    i32.add
  )

  ;; Constant folding candidate: should optimize to return 42
  (func $constant (export "constant") (result i32)
    i32.const 20
    i32.const 22
    i32.add
  )

  ;; Dead code after return
  (func $with_dead_code (export "with_dead_code") (param i32) (result i32)
    local.get 0
    return
    i32.const 999
    drop
    i32.const 0
  )

  ;; Memory operations
  (memory (export "memory") 1)

  ;; Store and load
  (func $store_load (export "store_load") (param i32 i32) (result i32)
    local.get 0     ;; address
    local.get 1     ;; value
    i32.store
    local.get 0     ;; address
    i32.load
  )
)
