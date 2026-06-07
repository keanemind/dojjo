Always refer to the Jujutsu source in the **`jj-<commit-sha>/`** directory that matches the tag in [`mise.toml`](mise.toml) (`jj_version`, currently **0.38.0** → tag `v0.38.0`). Populate it with **`mise run fetch-jj-ref`**. The tree is strictly read-only reference material, not part of dojjo's build; jj ignores fetched clones as nested git repos.

We use TigerBeetle's assertion style guide. Follow it strictly:

> **Assertions detect programmer errors. Unlike operating errors, which are expected and which must
> be handled, assertion failures are unexpected. The only correct way to handle corrupt code is to
> crash. Assertions downgrade catastrophic correctness bugs into liveness bugs. Assertions are a
> force multiplier for discovering bugs by fuzzing.**
>
> - **Assert all function arguments and return values, pre/postconditions and invariants.** A
>   function must not operate blindly on data it has not checked. The purpose of a function is to
>   increase the probability that a program is correct. Assertions within a function are part of how
>   functions serve this purpose. The assertion density of the code must average a minimum of two
>   assertions per function.
>
> - **[Pair assertions](https://tigerbeetle.com/blog/2023-12-27-it-takes-two-to-contract).** For
>   every property you want to enforce, try to find at least two different code paths where an
>   assertion can be added. For example, assert validity of data right before writing it to disk,
>   and also immediately after reading from disk.
>
> - On occasion, you may use a blatantly true assertion instead of a comment as stronger
>   documentation where the assertion condition is critical and surprising.
>
> - Split compound assertions: prefer `assert(a); assert(b);` over `assert(a and b);`.
>   The former is simpler to read, and provides more precise information if the condition fails.
>
> - Use single-line `if` to assert an implication: `if (a) assert(b)`.
>
> - **Assert the relationships of compile-time constants** as a sanity check, and also to document
>   and enforce [subtle
>   invariants](https://github.com/coilhq/tigerbeetle/blob/db789acfb93584e5cb9f331f9d6092ef90b53ea6/src/vsr/journal.zig#L45-L47)
>   or [type
>   sizes](https://github.com/coilhq/tigerbeetle/blob/578ac603326e1d3d33532701cb9285d5d2532fe7/src/ewah.zig#L41-L53).
>   Compile-time assertions are extremely powerful because they are able to check a program's design
>   integrity _before_ the program even executes.
>
> - **The golden rule of assertions is to assert the _positive space_ that you do expect AND to
>   assert the _negative space_ that you do not expect** because where data moves across the
>   valid/invalid boundary between these spaces is where interesting bugs are often found. This is
>   also why **tests must test exhaustively**, not only with valid data but also with invalid data,
>   and as valid data becomes invalid.
>
> - Assertions are a safety net, not a substitute for human understanding. With simulation testing,
>   there is the temptation to trust the fuzzer. But a fuzzer can prove only the presence of bugs,
>   not their absence. Therefore:
>   - Build a precise mental model of the code first,
>   - encode your understanding in the form of assertions,
>   - write the code and comments to explain and justify the mental model to your reviewer
