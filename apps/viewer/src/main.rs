fn main() -> anyhow::Result<()> {
    imperio_viewer::cli_main(std::env::args().skip(1).collect())
}
