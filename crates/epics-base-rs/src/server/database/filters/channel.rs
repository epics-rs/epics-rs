//! C `dbChannelCreate` (`dbChannel.c:448-530`): the one place a channel name
//! becomes a resolved `record.FIELD` plus its filter chain.

use super::{
    ChannelName, FilterChain, FilterParseError, parse_channel_name, try_parse_filter_chain,
};
use crate::server::database::PvDatabase;
use crate::types::{DbFieldType, EpicsValue};

/// A channel `dbChannelCreate` returned non-NULL for.
#[derive(Debug)]
pub struct DbChannel {
    pub name: ChannelName,
    pub chain: FilterChain,
}

impl DbChannel {
    /// `dbChannelFinalFieldType`: the field's own type as the chain's probe
    /// leaves it.
    pub fn final_field_type(&self, native: DbFieldType) -> DbFieldType {
        self.chain.final_field_type(native)
    }

    /// C `dbChannelGet` of the whole value through a read log: the field
    /// through the channel's filters, a `$` channel's chars as the string
    /// they hold. `None` where the read fails. Takes the record's lock; a
    /// caller must hold none.
    pub fn get(&self, db: &PvDatabase) -> Option<EpicsValue> {
        let read = match db.get_record(&self.name.record) {
            Some(rec) => {
                let field = rec.canonical_field(&self.name.field);
                crate::server::database::channel_io::record_field_snapshot_scan_locked(
                    db,
                    &rec,
                    &field,
                    self.name.string_view,
                )?
            }
            None => db.simple_pv_bytes(&self.name.record_path)?.snapshot(),
        };
        let value = read.value.clone();
        Some(
            self.chain
                .apply_to_read_snapshot(read)
                .map_or(value, |read| read.value),
        )
    }
}

/// Why `dbChannelCreate` returned NULL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelCreateError {
    /// `pvNameLookup` / `dbEntryToAddr` / the `$` test failed.
    NotFound,
    /// `chf_parse` stopped.
    Filter(FilterParseError),
}

impl std::fmt::Display for ChannelCreateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => f.write_str("channel not found"),
            Self::Filter(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for ChannelCreateError {}

impl ChannelCreateError {
    /// The line C's parse printed on the way to this NULL, if any:
    /// `chf_map_key`'s on an unknown filter (`dbChannel.c:179`), or yajl's
    /// block (`:295`), whose own trailing newline leaves C's blank line.
    pub fn report(&self) {
        match self {
            Self::Filter(FilterParseError::UnknownFilter { name }) => {
                crate::runtime::log::errlog_vprintf(format_args!(
                    "dbChannelCreate: Channel filter '{name}' not found\n"
                ));
            }
            Self::Filter(FilterParseError::Syntax { block, .. }) => {
                crate::runtime::log::errlog_vprintf(format_args!("dbChannelCreate: {block}\n"));
            }
            _ => {}
        }
    }
}

/// C `dbChannelCreate`, against the local database only (no search resolver).
///
/// The record and field resolve first, so a name that misses them never
/// reaches the filter parse and prints nothing; a failure is reported as C's
/// parse reports it, once per call. Takes record locks one at a time; a
/// caller must hold none.
pub fn db_channel_create(
    db: &PvDatabase,
    raw: impl AsRef<[u8]>,
) -> Result<DbChannel, ChannelCreateError> {
    let channel = db_channel_create_unreported(db, raw.as_ref());
    if let Err(e) = &channel {
        e.report();
    }
    channel
}

/// [`db_channel_create`] made ahead of the point C makes it, which then owes
/// [`ChannelCreateError::report`] exactly where C would have printed.
pub(crate) fn db_channel_create_unreported(
    db: &PvDatabase,
    raw: &[u8],
) -> Result<DbChannel, ChannelCreateError> {
    let name = parse_channel_name(raw);
    // An undotted `REC$` is the port's record-level long string, which C
    // refuses (see `split_channel_name`); its `$` is the caller's to judge.
    let probe = if name.string_view && !raw.contains(&b'.') {
        name.record_path.as_slice()
    } else {
        raw
    };
    if !db.resolve_channel_create(probe) {
        return Err(ChannelCreateError::NotFound);
    }
    let chain = match name.json_suffix.as_deref() {
        Some(json) => try_parse_filter_chain(json).map_err(ChannelCreateError::Filter)?,
        None => FilterChain::new(),
    };
    Ok(DbChannel { name, chain })
}
