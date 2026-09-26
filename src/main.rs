//! The `jj-cleanup` binary. All of it lives in the library so it can be tested.

fn main() -> anyhow::Result<()> {
    jj_cleanup::run()
}
