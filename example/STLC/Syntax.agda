module STLC.Syntax where

--! STLC >

open import Data.Nat

--! Syntax
data Expr : Set where
  `_   : ℕ → Expr
  λx_  : Expr → Expr
  _·_  : Expr → Expr → Expr

--! Examples {
example₁ : Expr
example₁ = λx (` 0)

example₂ : Expr
example₂ = λx (` 1)
--! }

inline-example : Expr → Expr
inline-example =
  --!! EAbs
  λx_

--! HideExample {
missing : Expr
missing =
--! [
  (` 0)
--! ]
  · (` 1)
--! }
