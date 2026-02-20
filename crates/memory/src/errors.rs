use types::MemoryError;

pub(crate) fn connection_error(message: String) -> MemoryError {
    MemoryError::Connection { message }
}

pub(crate) fn initialization_error(message: String) -> MemoryError {
    MemoryError::Initialization { message }
}

pub(crate) fn migration_error(message: String) -> MemoryError {
    MemoryError::Migration { message }
}

pub(crate) fn query_error(message: String) -> MemoryError {
    MemoryError::Query { message }
}
