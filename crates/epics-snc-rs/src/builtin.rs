//! Builtin constants and functions of SNL (C `builtin.c`).
//!
//! Analysis binds an undeclared name to one of these; [`Extra::Const`] and
//! [`Extra::Builtin`] hold the index into [`CONST_SYMBOLS`] or
//! [`FUNC_SYMBOLS`].
//!
//! [`Extra::Const`]: crate::types::Extra::Const
//! [`Extra::Builtin`]: crate::types::Extra::Builtin

use std::sync::Arc;

use crate::sym_table::SymTable;
use crate::types::{Sym, SymScope};

/// C `enum const_type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConstType {
    EvFlag,
    Other,
}

/// C `struct const_symbol`.
#[derive(Debug)]
pub struct ConstSymbol {
    pub name: &'static str,
    pub type_: ConstType,
}

/// C `enum param_type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamType {
    Ef,
    Pv,
    PvArray,
    Other,
}

/// C `struct param`.
#[derive(Debug)]
pub struct Param {
    pub type_: ParamType,
    pub default_value: Option<&'static str>,
}

/// C `struct func_symbol`.
#[derive(Debug)]
pub struct FuncSymbol {
    /// SNL name
    pub name: &'static str,
    /// C name, or `None` if same as SNL name
    pub c_name: Option<&'static str>,
    /// not allowed in when-conditions
    pub action_only: bool,
    /// only allowed in when-conditions
    pub cond_only: bool,
    /// parameter descriptions
    pub params: &'static [&'static Param],
}

macro_rules! consts {
    ($($name:literal $t:ident,)*) => {
        pub static CONST_SYMBOLS: &[ConstSymbol] = &[
            $(ConstSymbol { name: $name, type_: ConstType::$t },)*
        ];
    };
}

consts! {
    "TRUE" Other,
    "FALSE" Other,
    "SYNC" Other,
    "ASYNC" Other,
    "NOEVFLAG" EvFlag,
    "pvStatOK" Other,
    "pvStatERROR" Other,
    "pvStatDISCONN" Other,
    "pvStatREAD" Other,
    "pvStatWRITE" Other,
    "pvStatHIHI" Other,
    "pvStatHIGH" Other,
    "pvStatLOLO" Other,
    "pvStatLOW" Other,
    "pvStatSTATE" Other,
    "pvStatCOS" Other,
    "pvStatCOMM" Other,
    "pvStatTIMEOUT" Other,
    "pvStatHW_LIMIT" Other,
    "pvStatCALC" Other,
    "pvStatSCAN" Other,
    "pvStatLINK" Other,
    "pvStatSOFT" Other,
    "pvStatBAD_SUB" Other,
    "pvStatUDF" Other,
    "pvStatDISABLE" Other,
    "pvStatSIMM" Other,
    "pvStatREAD_ACCESS" Other,
    "pvStatWRITE_ACCESS" Other,
    "pvSevrOK" Other,
    "pvSevrERROR" Other,
    "pvSevrNONE" Other,
    "pvSevrMINOR" Other,
    "pvSevrMAJOR" Other,
    "pvSevrINVALID" Other,
    "seqg_var" Other,
    "seqg_env" Other,
}

// single parameter descriptors
const fn param(type_: ParamType, default_value: Option<&'static str>) -> Param {
    Param {
        type_,
        default_value,
    }
}
static EF_P: Param = param(ParamType::Ef, None);
static PV_P: Param = param(ParamType::Pv, None);
static PV_ARRAY_P: Param = param(ParamType::PvArray, None);
static NO_DEF_P: Param = param(ParamType::Other, None);
static COMP_TYPE_P: Param = param(ParamType::Other, Some("DEFAULT"));
static TMO_P: Param = param(ParamType::Other, Some("DEFAULT_TIMEOUT"));
static BOOL_P: Param = param(ParamType::Other, Some("FALSE"));
static PTR_P: Param = param(ParamType::Other, Some("NULL"));
static LENGTH_P: Param = param(ParamType::Other, None);
static DEF_LEN_P: Param = param(ParamType::Other, Some("1"));

// multiple parameter descriptors
static NO_PARAMS: [&Param; 0] = [];
static OTHER_PARAMS: [&Param; 1] = [&NO_DEF_P];
static EF_PARAMS: [&Param; 1] = [&EF_P];
static ASSIGN_PARAMS: [&Param; 2] = [&PV_P, &NO_DEF_P];
static PV_PARAMS: [&Param; 1] = [&PV_P];
static PV_ARRAY_PARAMS: [&Param; 2] = [&PV_ARRAY_P, &LENGTH_P];
static PV_SYNC_PARAMS: [&Param; 2] = [&PV_P, &EF_P];
static PV_ARRAY_SYNC_PARAMS: [&Param; 3] = [&PV_ARRAY_P, &LENGTH_P, &EF_P];
static PV_GET_PUT_PARAMS: [&Param; 3] = [&PV_P, &COMP_TYPE_P, &TMO_P];
static PV_ARRAY_GET_PUT_COMPLETE_PARAMS: [&Param; 4] = [&PV_ARRAY_P, &LENGTH_P, &BOOL_P, &PTR_P];
// for backward compatibility
static PV_PUT_COMPLETE_PARAMS: [&Param; 4] = [&PV_P, &DEF_LEN_P, &BOOL_P, &PTR_P];

macro_rules! funcs {
    ($($name:literal $c_name:expr, $action_only:literal $cond_only:literal $params:ident,)*) => {
        pub static FUNC_SYMBOLS: &[FuncSymbol] = &[
            $(FuncSymbol {
                name: $name,
                c_name: $c_name,
                action_only: $action_only,
                cond_only: $cond_only,
                params: &$params,
            },)*
        ];
    };
}

funcs! {
    // name                 c_name            action_only cond_only params
    "delay"                 None,             false true  OTHER_PARAMS,
    "efClear"               None,             true  false EF_PARAMS,
    "efSet"                 None,             true  false EF_PARAMS,
    "efTest"                None,             false false EF_PARAMS,
    "efTestAndClear"        None,             false false EF_PARAMS,
    "macValueGet"           None,             false false OTHER_PARAMS,
    "optGet"                None,             false false OTHER_PARAMS,
    "pvAssign"              None,             false false ASSIGN_PARAMS,
    "pvAssignCount"         None,             false false NO_PARAMS,
    "pvAssignSubst"         None,             false false ASSIGN_PARAMS,
    "pvAssigned"            None,             false false PV_PARAMS,
    "pvChannelCount"        None,             false false NO_PARAMS,
    "pvConnectCount"        None,             false false NO_PARAMS,
    "pvConnected"           None,             false false PV_PARAMS,
    "pvArrayConnected"      None,             false false PV_ARRAY_PARAMS,
    "pvCount"               None,             false false PV_PARAMS,
    "pvFlush"               None,             false false NO_PARAMS,
    "pvFlushQ"              None,             false false PV_PARAMS,
    "pvFreeQ"               None,             false false PV_PARAMS,
    "pvGet"                 Some("pvGetTmo"), false false PV_GET_PUT_PARAMS,
    "pvGetCancel"           None,             false false PV_PARAMS,
    "pvArrayGetCancel"      None,             false false PV_ARRAY_PARAMS,
    "pvGetComplete"         None,             false false PV_PARAMS,
    "pvArrayGetComplete"    None,             false false PV_ARRAY_GET_PUT_COMPLETE_PARAMS,
    "pvGetQ"                None,             false false PV_PARAMS,
    "pvIndex"               None,             false false PV_PARAMS,
    "pvMessage"             None,             false false PV_PARAMS,
    "pvMonitor"             None,             false false PV_PARAMS,
    "pvArrayMonitor"        None,             false false PV_ARRAY_PARAMS,
    "pvName"                None,             false false PV_PARAMS,
    "pvPut"                 Some("pvPutTmo"), false false PV_GET_PUT_PARAMS,
    "pvPutCancel"           None,             false false PV_PARAMS,
    "pvArrayPutCancel"      None,             false false PV_ARRAY_PARAMS,
    "pvPutComplete"         None,             false false PV_PUT_COMPLETE_PARAMS,
    "pvArrayPutComplete"    None,             false false PV_ARRAY_GET_PUT_COMPLETE_PARAMS,
    "pvSeverity"            None,             false false PV_PARAMS,
    "pvStatus"              None,             false false PV_PARAMS,
    "pvStopMonitor"         None,             false false PV_PARAMS,
    "pvArrayStopMonitor"    None,             false false PV_ARRAY_PARAMS,
    "pvSync"                None,             false false PV_SYNC_PARAMS,
    "pvArraySync"           None,             false false PV_ARRAY_SYNC_PARAMS,
    "pvTimeStamp"           None,             false false PV_PARAMS,
}

/// Insert builtin constants into symbol table.
pub fn register_builtin_consts(sym_table: &mut SymTable<SymScope, Sym>) {
    for (i, sym) in CONST_SYMBOLS.iter().enumerate() {
        sym_table.insert(&Arc::from(sym.name), SymScope::ConstSymbols, Sym::Const(i));
    }
}

/// Insert builtin functions into symbol table.
pub fn register_builtin_funcs(sym_table: &mut SymTable<SymScope, Sym>) {
    for (i, sym) in FUNC_SYMBOLS.iter().enumerate() {
        sym_table.insert(&Arc::from(sym.name), SymScope::FuncSymbols, Sym::Func(i));
    }
}

/// Look up a builtin function from the symbol table: its index in
/// [`FUNC_SYMBOLS`].
pub fn lookup_builtin_func(sym_table: &SymTable<SymScope, Sym>, func_name: &str) -> Option<usize> {
    match sym_table.lookup(func_name, SymScope::FuncSymbols)? {
        Sym::Func(i) => Some(i),
        _ => unreachable!("func_symbols holds functions only"),
    }
}

/// Look up a builtin constant from the symbol table: its index in
/// [`CONST_SYMBOLS`].
pub fn lookup_builtin_const(
    sym_table: &SymTable<SymScope, Sym>,
    const_name: &str,
) -> Option<usize> {
    match sym_table.lookup(const_name, SymScope::ConstSymbols)? {
        Sym::Const(i) => Some(i),
        _ => unreachable!("const_symbols holds constants only"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_match_builtin_c() {
        assert_eq!(CONST_SYMBOLS.len(), 37);
        assert_eq!(FUNC_SYMBOLS.len(), 42);
        let ef: Vec<_> = CONST_SYMBOLS
            .iter()
            .filter(|c| c.type_ == ConstType::EvFlag)
            .map(|c| c.name)
            .collect();
        assert_eq!(ef, ["NOEVFLAG"]);
        let get = &FUNC_SYMBOLS[19];
        assert_eq!((get.name, get.c_name), ("pvGet", Some("pvGetTmo")));
        let defaults: Vec<_> = get.params.iter().map(|p| p.default_value).collect();
        assert_eq!(defaults, [None, Some("DEFAULT"), Some("DEFAULT_TIMEOUT")]);
        let flags = |n| {
            let f = FUNC_SYMBOLS.iter().find(|f| f.name == n).unwrap();
            (f.action_only, f.cond_only)
        };
        assert_eq!(flags("delay"), (false, true));
        assert_eq!(flags("efSet"), (true, false));
        assert_eq!(flags("efClear"), (true, false));
        assert_eq!(flags("efTestAndClear"), (false, false));
    }

    #[test]
    fn lookups_use_separate_namespaces() {
        let mut st = SymTable::new();
        register_builtin_consts(&mut st);
        register_builtin_funcs(&mut st);
        assert_eq!(lookup_builtin_const(&st, "NOEVFLAG"), Some(4));
        assert_eq!(lookup_builtin_func(&st, "NOEVFLAG"), None);
        assert_eq!(lookup_builtin_func(&st, "pvTimeStamp"), Some(41));
        assert_eq!(lookup_builtin_const(&st, "pvPut"), None);
    }
}
