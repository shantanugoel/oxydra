/// Access mode for a path-bearing tool argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathAccessMode {
    ReadOnly,
    ReadWrite,
}

/// Path-bearing field metadata for a tool argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolPathField {
    pub field_name: &'static str,
    pub access_mode: PathAccessMode,
}

const EMPTY_FIELDS: &[ToolPathField] = &[];
const READ_ONLY_PATH_FIELD: &[ToolPathField] = &[ToolPathField {
    field_name: "path",
    access_mode: PathAccessMode::ReadOnly,
}];
const READ_WRITE_PATH_FIELD: &[ToolPathField] = &[ToolPathField {
    field_name: "path",
    access_mode: PathAccessMode::ReadWrite,
}];
const VAULT_COPYTO_FIELDS: &[ToolPathField] = &[
    ToolPathField {
        field_name: "source_path",
        access_mode: PathAccessMode::ReadOnly,
    },
    ToolPathField {
        field_name: "destination_path",
        access_mode: PathAccessMode::ReadWrite,
    },
];

/// Single source of truth for path-bearing tool arguments.
pub fn tool_path_fields(tool_name: &str) -> &'static [ToolPathField] {
    match tool_name {
        "file_read" | "file_search" | "file_list" | "send_media" => READ_ONLY_PATH_FIELD,
        "file_write" | "file_edit" | "file_delete" | "attachment_save" => READ_WRITE_PATH_FIELD,
        "vault_copyto" => VAULT_COPYTO_FIELDS,
        _ => EMPTY_FIELDS,
    }
}

#[cfg(test)]
mod tests {
    use super::{PathAccessMode, tool_path_fields};

    #[test]
    fn tool_path_fields_returns_expected_mapping() {
        assert_eq!(tool_path_fields("file_read").len(), 1);
        assert_eq!(tool_path_fields("file_read")[0].field_name, "path");
        assert_eq!(
            tool_path_fields("file_read")[0].access_mode,
            PathAccessMode::ReadOnly
        );

        assert_eq!(tool_path_fields("file_write").len(), 1);
        assert_eq!(tool_path_fields("file_write")[0].field_name, "path");
        assert_eq!(
            tool_path_fields("file_write")[0].access_mode,
            PathAccessMode::ReadWrite
        );

        assert_eq!(tool_path_fields("attachment_save").len(), 1);
        assert_eq!(tool_path_fields("attachment_save")[0].field_name, "path");
        assert_eq!(
            tool_path_fields("attachment_save")[0].access_mode,
            PathAccessMode::ReadWrite
        );

        assert_eq!(tool_path_fields("send_media").len(), 1);
        assert_eq!(tool_path_fields("send_media")[0].field_name, "path");
        assert_eq!(
            tool_path_fields("send_media")[0].access_mode,
            PathAccessMode::ReadOnly
        );

        let vault_fields = tool_path_fields("vault_copyto");
        assert_eq!(vault_fields.len(), 2);
        assert_eq!(vault_fields[0].field_name, "source_path");
        assert_eq!(vault_fields[0].access_mode, PathAccessMode::ReadOnly);
        assert_eq!(vault_fields[1].field_name, "destination_path");
        assert_eq!(vault_fields[1].access_mode, PathAccessMode::ReadWrite);

        assert!(tool_path_fields("unknown_tool").is_empty());
    }
}
