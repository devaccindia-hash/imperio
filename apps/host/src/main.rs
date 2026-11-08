fn main() -> anyhow::Result<()> {
    imperio_host::cli_main(std::env::args().skip(1).collect())
}
