(module
  (type (;0;) (func))
  (type (;1;) (func (result i32)))
  (type (;2;) (func (param i32) (result i32)))
  (type (;3;) (func (param i32)))
  (import "Mt" "call" (func (;0;) (type 2)))
  (import "Mt" "h" (func (;1;) (type 1)))
  (func (;2;) (type 1) (result i32)
    i32.const 5)
  (func (;3;) (type 2) (param i32) (result i32)
    local.get 0
    call 0)
  (func (;4;) (type 3) (param i32)
    local.get 0
    call_indirect (type 0))
  (table (;0;) 5 5 anyfunc)
  (export "Mt.call" (func 0))
  (export "call Mt.call" (func 3))
  (export "call" (func 4))
  (elem (;0;) (i32.const 0) 2 2 2 1 0))
