fn main() {
    // Deliberately exit before writing AIUM_UPDATE_READY_FILE. The helper must
    // treat this as a failed startup and restore the previous executable.
    std::process::exit(23);
}
