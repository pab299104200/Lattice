pub const CREATE_TABLES: &str = r#"
CREATE TABLE IF NOT EXISTS nodes (
    file TEXT NOT NULL,
    name TEXT NOT NULL,
    byte_offset INTEGER NOT NULL,
    kind TEXT NOT NULL,
    signature TEXT NOT NULL,
    body TEXT NOT NULL,
    line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    is_exported INTEGER NOT NULL,
    language TEXT NOT NULL,
    edit_count INTEGER NOT NULL DEFAULT 0,
    last_modified INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (file, name, byte_offset)
);

CREATE TABLE IF NOT EXISTS edges (
    from_file TEXT NOT NULL,
    from_name TEXT NOT NULL,
    from_offset INTEGER NOT NULL,
    to_file TEXT NOT NULL,
    to_name TEXT NOT NULL,
    to_offset INTEGER NOT NULL,
    kind TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS file_index (
    file TEXT PRIMARY KEY,
    content_hash TEXT NOT NULL,
    mtime_ns INTEGER NOT NULL,
    size_bytes INTEGER NOT NULL,
    parser_version INTEGER NOT NULL,
    schema_version INTEGER NOT NULL,
    last_indexed_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS parsed_files (
    file TEXT PRIMARY KEY,
    payload TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_nodes_file ON nodes(file);
CREATE INDEX IF NOT EXISTS idx_edges_from ON edges(from_file, from_name, from_offset);
CREATE INDEX IF NOT EXISTS idx_edges_to ON edges(to_file, to_name, to_offset);
"#;
