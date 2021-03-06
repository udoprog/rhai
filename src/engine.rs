//! Main module defining the script evaluation `Engine`.

use crate::any::{map_std_type_name, Dynamic, Union, Variant};
use crate::calc_fn_hash;
use crate::fn_call::run_builtin_op_assignment;
use crate::fn_native::{CallableFunction, Callback, FnPtr};
use crate::module::{resolvers, Module, ModuleRef, ModuleResolver};
use crate::optimize::OptimizationLevel;
use crate::packages::{Package, PackagesCollection, StandardPackage};
use crate::parser::{Expr, FnAccess, ImmutableString, ReturnType, ScriptFnDef, Stmt};
use crate::r#unsafe::unsafe_cast_var_name_to_lifetime;
use crate::result::EvalAltResult;
use crate::scope::{EntryType as ScopeEntryType, Scope};
use crate::syntax::{CustomSyntax, EvalContext, Expression};
use crate::token::Position;
use crate::utils::StaticVec;

use crate::stdlib::{
    any::TypeId,
    borrow::Cow,
    boxed::Box,
    collections::{HashMap, HashSet},
    fmt, format,
    iter::{empty, once},
    string::{String, ToString},
    vec::Vec,
};

/// Variable-sized array of `Dynamic` values.
///
/// Not available under the `no_index` feature.
#[cfg(not(feature = "no_index"))]
pub type Array = Vec<Dynamic>;

/// Hash map of `Dynamic` values with `ImmutableString` keys.
///
/// Not available under the `no_object` feature.
#[cfg(not(feature = "no_object"))]
pub type Map = HashMap<ImmutableString, Dynamic>;

/// A stack of imported modules.
pub type Imports<'a> = Vec<(Cow<'a, str>, Module)>;

#[cfg(not(feature = "unchecked"))]
#[cfg(debug_assertions)]
pub const MAX_CALL_STACK_DEPTH: usize = 16;
#[cfg(not(feature = "unchecked"))]
#[cfg(debug_assertions)]
pub const MAX_EXPR_DEPTH: usize = 32;
#[cfg(not(feature = "unchecked"))]
#[cfg(debug_assertions)]
pub const MAX_FUNCTION_EXPR_DEPTH: usize = 16;

#[cfg(not(feature = "unchecked"))]
#[cfg(not(debug_assertions))]
pub const MAX_CALL_STACK_DEPTH: usize = 128;
#[cfg(not(feature = "unchecked"))]
#[cfg(not(debug_assertions))]
pub const MAX_EXPR_DEPTH: usize = 128;
#[cfg(not(feature = "unchecked"))]
#[cfg(not(debug_assertions))]
pub const MAX_FUNCTION_EXPR_DEPTH: usize = 32;

#[cfg(feature = "unchecked")]
pub const MAX_CALL_STACK_DEPTH: usize = usize::MAX;
#[cfg(feature = "unchecked")]
pub const MAX_EXPR_DEPTH: usize = 0;
#[cfg(feature = "unchecked")]
pub const MAX_FUNCTION_EXPR_DEPTH: usize = 0;

pub const KEYWORD_PRINT: &str = "print";
pub const KEYWORD_DEBUG: &str = "debug";
pub const KEYWORD_TYPE_OF: &str = "type_of";
pub const KEYWORD_EVAL: &str = "eval";
pub const KEYWORD_FN_PTR: &str = "Fn";
pub const KEYWORD_FN_PTR_CALL: &str = "call";
pub const KEYWORD_FN_PTR_CURRY: &str = "curry";
pub const KEYWORD_THIS: &str = "this";
pub const FN_TO_STRING: &str = "to_string";
pub const FN_GET: &str = "get$";
pub const FN_SET: &str = "set$";
pub const FN_IDX_GET: &str = "index$get$";
pub const FN_IDX_SET: &str = "index$set$";
pub const FN_ANONYMOUS: &str = "anon$";
pub const MARKER_EXPR: &str = "$expr$";
pub const MARKER_BLOCK: &str = "$block$";
pub const MARKER_IDENT: &str = "$ident$";

/// A type specifying the method of chaining.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum ChainType {
    None,
    Index,
    Dot,
}

/// A type that encapsulates a mutation target for an expression with side effects.
#[derive(Debug)]
pub enum Target<'a> {
    /// The target is a mutable reference to a `Dynamic` value somewhere.
    Ref(&'a mut Dynamic),
    /// The target is a temporary `Dynamic` value (i.e. the mutation can cause no side effects).
    Value(Dynamic),
    /// The target is a character inside a String.
    /// This is necessary because directly pointing to a char inside a String is impossible.
    StringChar(&'a mut Dynamic, usize, Dynamic),
}

impl Target<'_> {
    /// Is the `Target` a reference pointing to other data?
    pub fn is_ref(&self) -> bool {
        match self {
            Self::Ref(_) => true,
            Self::Value(_) | Self::StringChar(_, _, _) => false,
        }
    }
    /// Is the `Target` an owned value?
    pub fn is_value(&self) -> bool {
        match self {
            Self::Ref(_) => false,
            Self::Value(_) => true,
            Self::StringChar(_, _, _) => false,
        }
    }
    /// Is the `Target` a specific type?
    pub fn is<T: Variant + Clone>(&self) -> bool {
        match self {
            Target::Ref(r) => r.is::<T>(),
            Target::Value(r) => r.is::<T>(),
            Target::StringChar(_, _, _) => TypeId::of::<T>() == TypeId::of::<char>(),
        }
    }
    /// Get the value of the `Target` as a `Dynamic`, cloning a referenced value if necessary.
    pub fn clone_into_dynamic(self) -> Dynamic {
        match self {
            Self::Ref(r) => r.clone(),        // Referenced value is cloned
            Self::Value(v) => v,              // Owned value is simply taken
            Self::StringChar(_, _, ch) => ch, // Character is taken
        }
    }
    /// Get a mutable reference from the `Target`.
    pub fn as_mut(&mut self) -> &mut Dynamic {
        match self {
            Self::Ref(r) => *r,
            Self::Value(ref mut r) => r,
            Self::StringChar(_, _, ref mut r) => r,
        }
    }
    /// Update the value of the `Target`.
    /// Position in `EvalAltResult` is `None` and must be set afterwards.
    pub fn set_value(&mut self, new_val: Dynamic) -> Result<(), Box<EvalAltResult>> {
        match self {
            Self::Ref(r) => **r = new_val,
            Self::Value(_) => {
                return Err(Box::new(EvalAltResult::ErrorAssignmentToUnknownLHS(
                    Position::none(),
                )))
            }
            Self::StringChar(Dynamic(Union::Str(ref mut s)), index, _) => {
                // Replace the character at the specified index position
                let new_ch = new_val
                    .as_char()
                    .map_err(|_| EvalAltResult::ErrorCharMismatch(Position::none()))?;

                let mut chars = s.chars().collect::<StaticVec<_>>();
                let ch = chars[*index];

                // See if changed - if so, update the String
                if ch != new_ch {
                    chars[*index] = new_ch;
                    *s = chars.iter().collect::<String>().into();
                }
            }
            _ => unreachable!(),
        }

        Ok(())
    }
}

impl<'a> From<&'a mut Dynamic> for Target<'a> {
    fn from(value: &'a mut Dynamic) -> Self {
        Self::Ref(value)
    }
}
impl<T: Into<Dynamic>> From<T> for Target<'_> {
    fn from(value: T) -> Self {
        Self::Value(value.into())
    }
}

/// A type that holds all the current states of the Engine.
///
/// # Safety
///
/// This type uses some unsafe code, mainly for avoiding cloning of local variable names via
/// direct lifetime casting.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Default)]
pub struct State {
    /// Normally, access to variables are parsed with a relative offset into the scope to avoid a lookup.
    /// In some situation, e.g. after running an `eval` statement, subsequent offsets become mis-aligned.
    /// When that happens, this flag is turned on to force a scope lookup by name.
    pub always_search: bool,
    /// Level of the current scope.  The global (root) level is zero, a new block (or function call)
    /// is one level higher, and so on.
    pub scope_level: usize,
    /// Number of operations performed.
    pub operations: u64,
    /// Number of modules loaded.
    pub modules: usize,
}

impl State {
    /// Create a new `State`.
    pub fn new() -> Self {
        Default::default()
    }
}

/// Get a script-defined function definition from a module.
#[cfg(not(feature = "no_function"))]
pub fn get_script_function_by_signature<'a>(
    module: &'a Module,
    name: &str,
    params: usize,
    public_only: bool,
) -> Option<&'a ScriptFnDef> {
    // Qualifiers (none) + function name + number of arguments.
    let hash_script = calc_fn_hash(empty(), name, params, empty());
    let func = module.get_fn(hash_script)?;
    if !func.is_script() {
        return None;
    }
    let fn_def = func.get_fn_def();

    match fn_def.access {
        FnAccess::Private if public_only => None,
        FnAccess::Private | FnAccess::Public => Some(&fn_def),
    }
}

/// Rhai main scripting engine.
///
/// ```
/// # fn main() -> Result<(), Box<rhai::EvalAltResult>> {
/// use rhai::Engine;
///
/// let engine = Engine::new();
///
/// let result = engine.eval::<i64>("40 + 2")?;
///
/// println!("Answer: {}", result);  // prints 42
/// # Ok(())
/// # }
/// ```
///
/// Currently, `Engine` is neither `Send` nor `Sync`. Use the `sync` feature to make it `Send + Sync`.
pub struct Engine {
    /// A unique ID identifying this scripting `Engine`.
    pub id: Option<String>,

    /// A module containing all functions directly loaded into the Engine.
    pub(crate) global_module: Module,
    /// A collection of all library packages loaded into the Engine.
    pub(crate) packages: PackagesCollection,

    /// A module resolution service.
    pub(crate) module_resolver: Option<Box<dyn ModuleResolver>>,

    /// A hashmap mapping type names to pretty-print names.
    pub(crate) type_names: Option<HashMap<String, String>>,

    /// A hashset containing symbols to disable.
    pub(crate) disabled_symbols: Option<HashSet<String>>,
    /// A hashset containing custom keywords and precedence to recognize.
    pub(crate) custom_keywords: Option<HashMap<String, u8>>,
    /// Custom syntax.
    pub(crate) custom_syntax: Option<HashMap<String, CustomSyntax>>,

    /// Callback closure for implementing the `print` command.
    pub(crate) print: Callback<str, ()>,
    /// Callback closure for implementing the `debug` command.
    pub(crate) debug: Callback<str, ()>,
    /// Callback closure for progress reporting.
    pub(crate) progress: Option<Callback<u64, bool>>,

    /// Optimize the AST after compilation.
    pub(crate) optimization_level: OptimizationLevel,
    /// Maximum levels of call-stack to prevent infinite recursion.
    ///
    /// Defaults to 16 for debug builds and 128 for non-debug builds.
    pub(crate) max_call_stack_depth: usize,
    /// Maximum depth of statements/expressions at global level.
    pub(crate) max_expr_depth: usize,
    /// Maximum depth of statements/expressions in functions.
    pub(crate) max_function_expr_depth: usize,
    /// Maximum number of operations allowed to run.
    pub(crate) max_operations: u64,
    /// Maximum number of modules allowed to load.
    pub(crate) max_modules: usize,
    /// Maximum length of a string.
    pub(crate) max_string_size: usize,
    /// Maximum length of an array.
    pub(crate) max_array_size: usize,
    /// Maximum number of properties in a map.
    pub(crate) max_map_size: usize,
}

impl fmt::Debug for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.id.as_ref() {
            Some(id) => write!(f, "Engine({})", id),
            None => f.write_str("Engine"),
        }
    }
}

impl Default for Engine {
    fn default() -> Self {
        // Create the new scripting Engine
        let mut engine = Self {
            id: None,

            packages: Default::default(),
            global_module: Default::default(),

            #[cfg(not(feature = "no_module"))]
            #[cfg(not(feature = "no_std"))]
            #[cfg(not(target_arch = "wasm32"))]
            module_resolver: Some(Box::new(resolvers::FileModuleResolver::new())),
            #[cfg(any(feature = "no_module", feature = "no_std", target_arch = "wasm32",))]
            module_resolver: None,

            type_names: None,
            disabled_symbols: None,
            custom_keywords: None,
            custom_syntax: None,

            // default print/debug implementations
            print: Box::new(default_print),
            debug: Box::new(default_print),

            // progress callback
            progress: None,

            // optimization level
            #[cfg(feature = "no_optimize")]
            optimization_level: OptimizationLevel::None,

            #[cfg(not(feature = "no_optimize"))]
            optimization_level: OptimizationLevel::Simple,

            max_call_stack_depth: MAX_CALL_STACK_DEPTH,
            max_expr_depth: MAX_EXPR_DEPTH,
            max_function_expr_depth: MAX_FUNCTION_EXPR_DEPTH,
            max_operations: 0,
            max_modules: usize::MAX,
            max_string_size: 0,
            max_array_size: 0,
            max_map_size: 0,
        };

        engine.load_package(StandardPackage::new().get());

        engine
    }
}

/// Make getter function
pub fn make_getter(id: &str) -> String {
    format!("{}{}", FN_GET, id)
}

/// Make setter function
pub fn make_setter(id: &str) -> String {
    format!("{}{}", FN_SET, id)
}

/// Print/debug to stdout
fn default_print(s: &str) {
    #[cfg(not(feature = "no_std"))]
    #[cfg(not(target_arch = "wasm32"))]
    println!("{}", s);
}

/// Search for a module within an imports stack.
/// Position in `EvalAltResult` is `None` and must be set afterwards.
pub fn search_imports<'s>(
    mods: &'s Imports,
    state: &mut State,
    modules: &Box<ModuleRef>,
) -> Result<&'s Module, Box<EvalAltResult>> {
    let (root, root_pos) = modules.get(0);

    // Qualified - check if the root module is directly indexed
    let index = if state.always_search {
        None
    } else {
        modules.index()
    };

    Ok(if let Some(index) = index {
        let offset = mods.len() - index.get();
        &mods.get(offset).unwrap().1
    } else {
        mods.iter()
            .rev()
            .find(|(n, _)| n == root)
            .map(|(_, m)| m)
            .ok_or_else(|| {
                Box::new(EvalAltResult::ErrorModuleNotFound(
                    root.to_string(),
                    *root_pos,
                ))
            })?
    })
}

/// Search for a module within an imports stack.
/// Position in `EvalAltResult` is `None` and must be set afterwards.
pub fn search_imports_mut<'s>(
    mods: &'s mut Imports,
    state: &mut State,
    modules: &Box<ModuleRef>,
) -> Result<&'s mut Module, Box<EvalAltResult>> {
    let (root, root_pos) = modules.get(0);

    // Qualified - check if the root module is directly indexed
    let index = if state.always_search {
        None
    } else {
        modules.index()
    };

    Ok(if let Some(index) = index {
        let offset = mods.len() - index.get();
        &mut mods.get_mut(offset).unwrap().1
    } else {
        mods.iter_mut()
            .rev()
            .find(|(n, _)| n == root)
            .map(|(_, m)| m)
            .ok_or_else(|| {
                Box::new(EvalAltResult::ErrorModuleNotFound(
                    root.to_string(),
                    *root_pos,
                ))
            })?
    })
}

/// Search for a variable within the scope and imports
pub fn search_namespace<'s, 'a>(
    scope: &'s mut Scope,
    mods: &'s mut Imports,
    state: &mut State,
    this_ptr: &'s mut Option<&mut Dynamic>,
    expr: &'a Expr,
) -> Result<(&'s mut Dynamic, &'a str, ScopeEntryType, Position), Box<EvalAltResult>> {
    match expr {
        Expr::Variable(v) => match v.as_ref() {
            // Qualified variable
            ((name, pos), Some(modules), hash_var, _) => {
                let module = search_imports_mut(mods, state, modules)?;
                let target = module
                    .get_qualified_var_mut(*hash_var)
                    .map_err(|err| match *err {
                        EvalAltResult::ErrorVariableNotFound(_, _) => {
                            Box::new(EvalAltResult::ErrorVariableNotFound(
                                format!("{}{}", modules, name),
                                *pos,
                            ))
                        }
                        _ => err.new_position(*pos),
                    })?;

                // Module variables are constant
                Ok((target, name, ScopeEntryType::Constant, *pos))
            }
            // Normal variable access
            _ => search_scope_only(scope, state, this_ptr, expr),
        },
        _ => unreachable!(),
    }
}

/// Search for a variable within the scope
pub fn search_scope_only<'s, 'a>(
    scope: &'s mut Scope,
    state: &mut State,
    this_ptr: &'s mut Option<&mut Dynamic>,
    expr: &'a Expr,
) -> Result<(&'s mut Dynamic, &'a str, ScopeEntryType, Position), Box<EvalAltResult>> {
    let ((name, pos), _, _, index) = match expr {
        Expr::Variable(v) => v.as_ref(),
        _ => unreachable!(),
    };

    // Check if the variable is `this`
    if name == KEYWORD_THIS {
        if let Some(val) = this_ptr {
            return Ok(((*val).into(), KEYWORD_THIS, ScopeEntryType::Normal, *pos));
        } else {
            return Err(Box::new(EvalAltResult::ErrorUnboundedThis(*pos)));
        }
    }

    // Check if it is directly indexed
    let index = if state.always_search { None } else { *index };

    let index = if let Some(index) = index {
        scope.len() - index.get()
    } else {
        // Find the variable in the scope
        scope
            .get_index(name)
            .ok_or_else(|| Box::new(EvalAltResult::ErrorVariableNotFound(name.into(), *pos)))?
            .0
    };

    let (val, typ) = scope.get_mut(index);
    Ok((val, name, typ, *pos))
}

impl Engine {
    /// Create a new `Engine`
    pub fn new() -> Self {
        Default::default()
    }

    /// Create a new `Engine` with minimal built-in functions.
    /// Use the `load_package` method to load additional packages of functions.
    pub fn new_raw() -> Self {
        Self {
            id: None,

            packages: Default::default(),
            global_module: Default::default(),
            module_resolver: None,

            type_names: None,
            disabled_symbols: None,
            custom_keywords: None,
            custom_syntax: None,

            print: Box::new(|_| {}),
            debug: Box::new(|_| {}),
            progress: None,

            #[cfg(feature = "no_optimize")]
            optimization_level: OptimizationLevel::None,

            #[cfg(not(feature = "no_optimize"))]
            optimization_level: OptimizationLevel::Simple,

            max_call_stack_depth: MAX_CALL_STACK_DEPTH,
            max_expr_depth: MAX_EXPR_DEPTH,
            max_function_expr_depth: MAX_FUNCTION_EXPR_DEPTH,
            max_operations: 0,
            max_modules: usize::MAX,
            max_string_size: 0,
            max_array_size: 0,
            max_map_size: 0,
        }
    }

    /// Chain-evaluate a dot/index chain.
    /// Position in `EvalAltResult` is `None` and must be set afterwards.
    fn eval_dot_index_chain_helper(
        &self,
        state: &mut State,
        lib: &Module,
        this_ptr: &mut Option<&mut Dynamic>,
        target: &mut Target,
        rhs: &Expr,
        idx_values: &mut StaticVec<Dynamic>,
        chain_type: ChainType,
        level: usize,
        mut new_val: Option<Dynamic>,
    ) -> Result<(Dynamic, bool), Box<EvalAltResult>> {
        if chain_type == ChainType::None {
            panic!();
        }

        let is_ref = target.is_ref();

        let next_chain = match rhs {
            Expr::Index(_) => ChainType::Index,
            Expr::Dot(_) => ChainType::Dot,
            _ => ChainType::None,
        };

        // Pop the last index value
        let idx_val = idx_values.pop();

        match chain_type {
            #[cfg(not(feature = "no_index"))]
            ChainType::Index => {
                let pos = rhs.position();

                match rhs {
                    // xxx[idx].expr... | xxx[idx][expr]...
                    Expr::Dot(x) | Expr::Index(x) => {
                        let (idx, expr, pos) = x.as_ref();
                        let idx_pos = idx.position();
                        let obj_ptr = &mut self
                            .get_indexed_mut(state, lib, target, idx_val, idx_pos, false, level)?;

                        self.eval_dot_index_chain_helper(
                            state, lib, this_ptr, obj_ptr, expr, idx_values, next_chain, level,
                            new_val,
                        )
                        .map_err(|err| err.new_position(*pos))
                    }
                    // xxx[rhs] = new_val
                    _ if new_val.is_some() => {
                        let mut new_val = new_val.unwrap();
                        let mut idx_val2 = idx_val.clone();

                        match self.get_indexed_mut(state, lib, target, idx_val, pos, true, level) {
                            // Indexed value is an owned value - the only possibility is an indexer
                            // Try to call an index setter
                            Ok(obj_ptr) if obj_ptr.is_value() => {
                                let args = &mut [target.as_mut(), &mut idx_val2, &mut new_val];

                                self.exec_fn_call(
                                    state, lib, FN_IDX_SET, true, 0, args, is_ref, true, None,
                                    level,
                                )
                                .or_else(|err| match *err {
                                    // If there is no index setter, no need to set it back because the indexer is read-only
                                    EvalAltResult::ErrorFunctionNotFound(s, _)
                                        if s == FN_IDX_SET =>
                                    {
                                        Ok(Default::default())
                                    }
                                    _ => Err(err),
                                })?;
                            }
                            // Indexed value is a reference - update directly
                            Ok(ref mut obj_ptr) => {
                                obj_ptr
                                    .set_value(new_val)
                                    .map_err(|err| err.new_position(rhs.position()))?;
                            }
                            Err(err) => match *err {
                                // No index getter - try to call an index setter
                                EvalAltResult::ErrorIndexingType(_, _) => {
                                    let args = &mut [target.as_mut(), &mut idx_val2, &mut new_val];

                                    self.exec_fn_call(
                                        state, lib, FN_IDX_SET, true, 0, args, is_ref, true, None,
                                        level,
                                    )?;
                                }
                                // Error
                                err => return Err(Box::new(err)),
                            },
                        }
                        Ok(Default::default())
                    }
                    // xxx[rhs]
                    _ => self
                        .get_indexed_mut(state, lib, target, idx_val, pos, false, level)
                        .map(|v| (v.clone_into_dynamic(), false)),
                }
            }

            #[cfg(not(feature = "no_object"))]
            ChainType::Dot => {
                match rhs {
                    // xxx.fn_name(arg_expr_list)
                    Expr::FnCall(x) if x.1.is_none() => {
                        self.make_method_call(state, lib, target, rhs, idx_val, level)
                    }
                    // xxx.module::fn_name(...) - syntax error
                    Expr::FnCall(_) => unreachable!(),
                    // {xxx:map}.id = ???
                    Expr::Property(x) if target.is::<Map>() && new_val.is_some() => {
                        let ((prop, _, _), pos) = x.as_ref();
                        let index = prop.clone().into();
                        let mut val =
                            self.get_indexed_mut(state, lib, target, index, *pos, true, level)?;

                        val.set_value(new_val.unwrap())
                            .map_err(|err| err.new_position(rhs.position()))?;
                        Ok((Default::default(), true))
                    }
                    // {xxx:map}.id
                    Expr::Property(x) if target.is::<Map>() => {
                        let ((prop, _, _), pos) = x.as_ref();
                        let index = prop.clone().into();
                        let val =
                            self.get_indexed_mut(state, lib, target, index, *pos, false, level)?;

                        Ok((val.clone_into_dynamic(), false))
                    }
                    // xxx.id = ???
                    Expr::Property(x) if new_val.is_some() => {
                        let ((_, _, setter), pos) = x.as_ref();
                        let mut args = [target.as_mut(), new_val.as_mut().unwrap()];
                        self.exec_fn_call(
                            state, lib, setter, true, 0, &mut args, is_ref, true, None, level,
                        )
                        .map(|(v, _)| (v, true))
                        .map_err(|err| err.new_position(*pos))
                    }
                    // xxx.id
                    Expr::Property(x) => {
                        let ((_, getter, _), pos) = x.as_ref();
                        let mut args = [target.as_mut()];
                        self.exec_fn_call(
                            state, lib, getter, true, 0, &mut args, is_ref, true, None, level,
                        )
                        .map(|(v, _)| (v, false))
                        .map_err(|err| err.new_position(*pos))
                    }
                    // {xxx:map}.sub_lhs[expr] | {xxx:map}.sub_lhs.expr
                    Expr::Index(x) | Expr::Dot(x) if target.is::<Map>() => {
                        let (sub_lhs, expr, pos) = x.as_ref();

                        let mut val = match sub_lhs {
                            Expr::Property(p) => {
                                let ((prop, _, _), _) = p.as_ref();
                                let index = prop.clone().into();
                                self.get_indexed_mut(state, lib, target, index, *pos, false, level)?
                            }
                            // {xxx:map}.fn_name(arg_expr_list)[expr] | {xxx:map}.fn_name(arg_expr_list).expr
                            Expr::FnCall(x) if x.1.is_none() => {
                                let (val, _) = self.make_method_call(
                                    state, lib, target, sub_lhs, idx_val, level,
                                )?;
                                val.into()
                            }
                            // {xxx:map}.module::fn_name(...) - syntax error
                            Expr::FnCall(_) => unreachable!(),
                            // Others - syntax error
                            _ => unreachable!(),
                        };

                        self.eval_dot_index_chain_helper(
                            state, lib, this_ptr, &mut val, expr, idx_values, next_chain, level,
                            new_val,
                        )
                        .map_err(|err| err.new_position(*pos))
                    }
                    // xxx.sub_lhs[expr] | xxx.sub_lhs.expr
                    Expr::Index(x) | Expr::Dot(x) => {
                        let (sub_lhs, expr, pos) = x.as_ref();

                        match sub_lhs {
                            // xxx.prop[expr] | xxx.prop.expr
                            Expr::Property(p) => {
                                let ((_, getter, setter), _) = p.as_ref();
                                let arg_values = &mut [target.as_mut(), &mut Default::default()];
                                let args = &mut arg_values[..1];
                                let (mut val, updated) = self
                                    .exec_fn_call(
                                        state, lib, getter, true, 0, args, is_ref, true, None,
                                        level,
                                    )
                                    .map_err(|err| err.new_position(*pos))?;

                                let val = &mut val;
                                let target = &mut val.into();

                                let (result, may_be_changed) = self
                                    .eval_dot_index_chain_helper(
                                        state, lib, this_ptr, target, expr, idx_values, next_chain,
                                        level, new_val,
                                    )
                                    .map_err(|err| err.new_position(*pos))?;

                                // Feed the value back via a setter just in case it has been updated
                                if updated || may_be_changed {
                                    // Re-use args because the first &mut parameter will not be consumed
                                    arg_values[1] = val;
                                    self.exec_fn_call(
                                        state, lib, setter, true, 0, arg_values, is_ref, true,
                                        None, level,
                                    )
                                    .or_else(
                                        |err| match *err {
                                            // If there is no setter, no need to feed it back because the property is read-only
                                            EvalAltResult::ErrorDotExpr(_, _) => {
                                                Ok(Default::default())
                                            }
                                            _ => Err(err.new_position(*pos)),
                                        },
                                    )?;
                                }

                                Ok((result, may_be_changed))
                            }
                            // xxx.fn_name(arg_expr_list)[expr] | xxx.fn_name(arg_expr_list).expr
                            Expr::FnCall(x) if x.1.is_none() => {
                                let (mut val, _) = self.make_method_call(
                                    state, lib, target, sub_lhs, idx_val, level,
                                )?;
                                let val = &mut val;
                                let target = &mut val.into();

                                self.eval_dot_index_chain_helper(
                                    state, lib, this_ptr, target, expr, idx_values, next_chain,
                                    level, new_val,
                                )
                                .map_err(|err| err.new_position(*pos))
                            }
                            // xxx.module::fn_name(...) - syntax error
                            Expr::FnCall(_) => unreachable!(),
                            // Others - syntax error
                            _ => unreachable!(),
                        }
                    }
                    // Syntax error
                    _ => Err(Box::new(EvalAltResult::ErrorDotExpr(
                        "".into(),
                        rhs.position(),
                    ))),
                }
            }

            _ => unreachable!(),
        }
    }

    /// Evaluate a dot/index chain.
    fn eval_dot_index_chain(
        &self,
        scope: &mut Scope,
        mods: &mut Imports,
        state: &mut State,
        lib: &Module,
        this_ptr: &mut Option<&mut Dynamic>,
        expr: &Expr,
        level: usize,
        new_val: Option<Dynamic>,
    ) -> Result<Dynamic, Box<EvalAltResult>> {
        let ((dot_lhs, dot_rhs, op_pos), chain_type) = match expr {
            Expr::Index(x) => (x.as_ref(), ChainType::Index),
            Expr::Dot(x) => (x.as_ref(), ChainType::Dot),
            _ => unreachable!(),
        };

        let idx_values = &mut StaticVec::new();

        self.eval_indexed_chain(
            scope, mods, state, lib, this_ptr, dot_rhs, chain_type, idx_values, 0, level,
        )?;

        match dot_lhs {
            // id.??? or id[???]
            Expr::Variable(x) => {
                let (var_name, var_pos) = &x.0;

                self.inc_operations(state)
                    .map_err(|err| err.new_position(*var_pos))?;

                let (target, _, typ, pos) =
                    search_namespace(scope, mods, state, this_ptr, dot_lhs)?;

                // Constants cannot be modified
                match typ {
                    ScopeEntryType::Constant if new_val.is_some() => {
                        return Err(Box::new(EvalAltResult::ErrorAssignmentToConstant(
                            var_name.to_string(),
                            pos,
                        )));
                    }
                    ScopeEntryType::Constant | ScopeEntryType::Normal => (),
                }

                let obj_ptr = &mut target.into();
                self.eval_dot_index_chain_helper(
                    state, lib, &mut None, obj_ptr, dot_rhs, idx_values, chain_type, level, new_val,
                )
                .map(|(v, _)| v)
                .map_err(|err| err.new_position(*op_pos))
            }
            // {expr}.??? = ??? or {expr}[???] = ???
            expr if new_val.is_some() => {
                return Err(Box::new(EvalAltResult::ErrorAssignmentToUnknownLHS(
                    expr.position(),
                )));
            }
            // {expr}.??? or {expr}[???]
            expr => {
                let val = self.eval_expr(scope, mods, state, lib, this_ptr, expr, level)?;
                let obj_ptr = &mut val.into();
                self.eval_dot_index_chain_helper(
                    state, lib, this_ptr, obj_ptr, dot_rhs, idx_values, chain_type, level, new_val,
                )
                .map(|(v, _)| v)
                .map_err(|err| err.new_position(*op_pos))
            }
        }
    }

    /// Evaluate a chain of indexes and store the results in a list.
    /// The first few results are stored in the array `list` which is of fixed length.
    /// Any spill-overs are stored in `more`, which is dynamic.
    /// The fixed length array is used to avoid an allocation in the overwhelming cases of just a few levels of indexing.
    /// The total number of values is returned.
    fn eval_indexed_chain(
        &self,
        scope: &mut Scope,
        mods: &mut Imports,
        state: &mut State,
        lib: &Module,
        this_ptr: &mut Option<&mut Dynamic>,
        expr: &Expr,
        chain_type: ChainType,
        idx_values: &mut StaticVec<Dynamic>,
        size: usize,
        level: usize,
    ) -> Result<(), Box<EvalAltResult>> {
        self.inc_operations(state)
            .map_err(|err| err.new_position(expr.position()))?;

        match expr {
            Expr::FnCall(x) if x.1.is_none() => {
                let arg_values =
                    x.3.iter()
                        .map(|arg_expr| {
                            self.eval_expr(scope, mods, state, lib, this_ptr, arg_expr, level)
                        })
                        .collect::<Result<StaticVec<Dynamic>, _>>()?;

                idx_values.push(Dynamic::from(arg_values));
            }
            Expr::FnCall(_) => unreachable!(),
            Expr::Property(_) => idx_values.push(()), // Store a placeholder - no need to copy the property name
            Expr::Index(x) | Expr::Dot(x) => {
                let (lhs, rhs, _) = x.as_ref();

                // Evaluate in left-to-right order
                let lhs_val = match lhs {
                    Expr::Property(_) => Default::default(), // Store a placeholder in case of a property
                    Expr::FnCall(x) if chain_type == ChainType::Dot && x.1.is_none() => {
                        let arg_values = x
                            .3
                            .iter()
                            .map(|arg_expr| {
                                self.eval_expr(scope, mods, state, lib, this_ptr, arg_expr, level)
                            })
                            .collect::<Result<StaticVec<Dynamic>, _>>()?;

                        Dynamic::from(arg_values)
                    }
                    Expr::FnCall(_) => unreachable!(),
                    _ => self.eval_expr(scope, mods, state, lib, this_ptr, lhs, level)?,
                };

                // Push in reverse order
                let chain_type = match expr {
                    Expr::Index(_) => ChainType::Index,
                    Expr::Dot(_) => ChainType::Dot,
                    _ => unreachable!(),
                };
                self.eval_indexed_chain(
                    scope, mods, state, lib, this_ptr, rhs, chain_type, idx_values, size, level,
                )?;

                idx_values.push(lhs_val);
            }
            _ => idx_values.push(self.eval_expr(scope, mods, state, lib, this_ptr, expr, level)?),
        }

        Ok(())
    }

    /// Get the value at the indexed position of a base type
    /// Position in `EvalAltResult` may be None and should be set afterwards.
    fn get_indexed_mut<'a>(
        &self,
        state: &mut State,
        lib: &Module,
        target: &'a mut Target,
        mut idx: Dynamic,
        idx_pos: Position,
        create: bool,
        level: usize,
    ) -> Result<Target<'a>, Box<EvalAltResult>> {
        self.inc_operations(state)?;

        let is_ref = target.is_ref();
        let val = target.as_mut();

        match val {
            #[cfg(not(feature = "no_index"))]
            Dynamic(Union::Array(arr)) => {
                // val_array[idx]
                let index = idx
                    .as_int()
                    .map_err(|_| EvalAltResult::ErrorNumericIndexExpr(idx_pos))?;

                let arr_len = arr.len();

                if index >= 0 {
                    arr.get_mut(index as usize)
                        .map(Target::from)
                        .ok_or_else(|| {
                            Box::new(EvalAltResult::ErrorArrayBounds(arr_len, index, idx_pos))
                        })
                } else {
                    Err(Box::new(EvalAltResult::ErrorArrayBounds(
                        arr_len, index, idx_pos,
                    )))
                }
            }

            #[cfg(not(feature = "no_object"))]
            Dynamic(Union::Map(map)) => {
                // val_map[idx]
                Ok(if create {
                    let index = idx
                        .take_immutable_string()
                        .map_err(|_| EvalAltResult::ErrorStringIndexExpr(idx_pos))?;

                    map.entry(index).or_insert(Default::default()).into()
                } else {
                    let index = idx
                        .downcast_ref::<String>()
                        .ok_or_else(|| EvalAltResult::ErrorStringIndexExpr(idx_pos))?;

                    map.get_mut(index.as_str())
                        .map(Target::from)
                        .unwrap_or_else(|| Target::from(()))
                })
            }

            #[cfg(not(feature = "no_index"))]
            Dynamic(Union::Str(s)) => {
                // val_string[idx]
                let chars_len = s.chars().count();
                let index = idx
                    .as_int()
                    .map_err(|_| EvalAltResult::ErrorNumericIndexExpr(idx_pos))?;

                if index >= 0 {
                    let offset = index as usize;
                    let ch = s.chars().nth(offset).ok_or_else(|| {
                        Box::new(EvalAltResult::ErrorStringBounds(chars_len, index, idx_pos))
                    })?;
                    Ok(Target::StringChar(val, offset, ch.into()))
                } else {
                    Err(Box::new(EvalAltResult::ErrorStringBounds(
                        chars_len, index, idx_pos,
                    )))
                }
            }

            #[cfg(not(feature = "no_index"))]
            _ => {
                let val_type_name = val.type_name();
                let args = &mut [val, &mut idx];
                self.exec_fn_call(
                    state, lib, FN_IDX_GET, true, 0, args, is_ref, true, None, level,
                )
                .map(|(v, _)| v.into())
                .map_err(|e| match *e {
                    EvalAltResult::ErrorFunctionNotFound(..) => {
                        Box::new(EvalAltResult::ErrorIndexingType(
                            self.map_type_name(val_type_name).into(),
                            Position::none(),
                        ))
                    }
                    _ => e,
                })
            }

            #[cfg(feature = "no_index")]
            _ => Err(Box::new(EvalAltResult::ErrorIndexingType(
                self.map_type_name(val.type_name()).into(),
                Position::none(),
            ))),
        }
    }

    // Evaluate an 'in' expression
    fn eval_in_expr(
        &self,
        scope: &mut Scope,
        mods: &mut Imports,
        state: &mut State,
        lib: &Module,
        this_ptr: &mut Option<&mut Dynamic>,
        lhs: &Expr,
        rhs: &Expr,
        level: usize,
    ) -> Result<Dynamic, Box<EvalAltResult>> {
        self.inc_operations(state)
            .map_err(|err| err.new_position(rhs.position()))?;

        let lhs_value = self.eval_expr(scope, mods, state, lib, this_ptr, lhs, level)?;
        let rhs_value = self.eval_expr(scope, mods, state, lib, this_ptr, rhs, level)?;

        match rhs_value {
            #[cfg(not(feature = "no_index"))]
            Dynamic(Union::Array(mut rhs_value)) => {
                let op = "==";
                let mut scope = Scope::new();

                // Call the `==` operator to compare each value
                for value in rhs_value.iter_mut() {
                    let def_value = Some(false);
                    let args = &mut [&mut lhs_value.clone(), value];

                    let hashes = (
                        // Qualifiers (none) + function name + number of arguments + argument `TypeId`'s.
                        calc_fn_hash(empty(), op, args.len(), args.iter().map(|a| a.type_id())),
                        0,
                    );

                    let (r, _) = self
                        .call_fn_raw(
                            &mut scope, mods, state, lib, op, hashes, args, false, false,
                            def_value, level,
                        )
                        .map_err(|err| err.new_position(rhs.position()))?;
                    if r.as_bool().unwrap_or(false) {
                        return Ok(true.into());
                    }
                }

                Ok(false.into())
            }
            #[cfg(not(feature = "no_object"))]
            Dynamic(Union::Map(rhs_value)) => match lhs_value {
                // Only allows String or char
                Dynamic(Union::Str(s)) => Ok(rhs_value.contains_key(s.as_str()).into()),
                Dynamic(Union::Char(c)) => {
                    Ok(rhs_value.contains_key(c.to_string().as_str()).into())
                }
                _ => Err(Box::new(EvalAltResult::ErrorInExpr(lhs.position()))),
            },
            Dynamic(Union::Str(rhs_value)) => match lhs_value {
                // Only allows String or char
                Dynamic(Union::Str(s)) => Ok(rhs_value.contains(s.as_str()).into()),
                Dynamic(Union::Char(c)) => Ok(rhs_value.contains(c).into()),
                _ => Err(Box::new(EvalAltResult::ErrorInExpr(lhs.position()))),
            },
            _ => Err(Box::new(EvalAltResult::ErrorInExpr(rhs.position()))),
        }
    }

    /// Evaluate an expression
    pub(crate) fn eval_expr(
        &self,
        scope: &mut Scope,
        mods: &mut Imports,
        state: &mut State,
        lib: &Module,
        this_ptr: &mut Option<&mut Dynamic>,
        expr: &Expr,
        level: usize,
    ) -> Result<Dynamic, Box<EvalAltResult>> {
        self.inc_operations(state)
            .map_err(|err| err.new_position(expr.position()))?;

        let result = match expr {
            Expr::Expr(x) => self.eval_expr(scope, mods, state, lib, this_ptr, x.as_ref(), level),

            Expr::IntegerConstant(x) => Ok(x.0.into()),
            #[cfg(not(feature = "no_float"))]
            Expr::FloatConstant(x) => Ok(x.0.into()),
            Expr::StringConstant(x) => Ok(x.0.to_string().into()),
            Expr::CharConstant(x) => Ok(x.0.into()),
            Expr::FnPointer(x) => Ok(FnPtr::new_unchecked(x.0.clone(), Default::default()).into()),
            Expr::Variable(x) if (x.0).0 == KEYWORD_THIS => {
                if let Some(val) = this_ptr {
                    Ok(val.clone())
                } else {
                    Err(Box::new(EvalAltResult::ErrorUnboundedThis((x.0).1)))
                }
            }
            Expr::Variable(_) => {
                let (val, _, _, _) = search_namespace(scope, mods, state, this_ptr, expr)?;
                Ok(val.clone())
            }
            Expr::Property(_) => unreachable!(),

            // Statement block
            Expr::Stmt(x) => self.eval_stmt(scope, mods, state, lib, this_ptr, &x.0, level),

            // var op= rhs
            Expr::Assignment(x) if matches!(x.0, Expr::Variable(_)) => {
                let (lhs_expr, op, rhs_expr, op_pos) = x.as_ref();
                let mut rhs_val =
                    self.eval_expr(scope, mods, state, lib, this_ptr, rhs_expr, level)?;
                let (lhs_ptr, name, typ, pos) =
                    search_namespace(scope, mods, state, this_ptr, lhs_expr)?;
                self.inc_operations(state)
                    .map_err(|err| err.new_position(pos))?;

                match typ {
                    // Assignment to constant variable
                    ScopeEntryType::Constant => Err(Box::new(
                        EvalAltResult::ErrorAssignmentToConstant(name.to_string(), pos),
                    )),
                    // Normal assignment
                    ScopeEntryType::Normal if op.is_empty() => {
                        *lhs_ptr = rhs_val;
                        Ok(Default::default())
                    }
                    // Op-assignment - in order of precedence:
                    ScopeEntryType::Normal => {
                        // 1) Native registered overriding function
                        // 2) Built-in implementation
                        // 3) Map to `var = var op rhs`

                        // Qualifiers (none) + function name + number of arguments + argument `TypeId`'s.
                        let arg_types = once(lhs_ptr.type_id()).chain(once(rhs_val.type_id()));
                        let hash_fn = calc_fn_hash(empty(), op, 2, arg_types);

                        if let Some(CallableFunction::Method(func)) = self
                            .global_module
                            .get_fn(hash_fn)
                            .or_else(|| self.packages.get_fn(hash_fn))
                        {
                            // Overriding exact implementation
                            func(self, lib, &mut [lhs_ptr, &mut rhs_val])?;
                        } else if run_builtin_op_assignment(op, lhs_ptr, &rhs_val)?.is_none() {
                            // Not built in, map to `var = var op rhs`
                            let op = &op[..op.len() - 1]; // extract operator without =
                            let hash = calc_fn_hash(empty(), op, 2, empty());
                            // Clone the LHS value
                            let args = &mut [&mut lhs_ptr.clone(), &mut rhs_val];
                            // Run function
                            let (value, _) = self
                                .exec_fn_call(
                                    state, lib, op, true, hash, args, false, false, None, level,
                                )
                                .map_err(|err| err.new_position(*op_pos))?;
                            // Set value to LHS
                            *lhs_ptr = value;
                        }
                        Ok(Default::default())
                    }
                }
            }

            // lhs op= rhs
            Expr::Assignment(x) => {
                let (lhs_expr, op, rhs_expr, op_pos) = x.as_ref();
                let mut rhs_val =
                    self.eval_expr(scope, mods, state, lib, this_ptr, rhs_expr, level)?;

                let new_val = Some(if op.is_empty() {
                    // Normal assignment
                    rhs_val
                } else {
                    // Op-assignment - always map to `lhs = lhs op rhs`
                    let op = &op[..op.len() - 1]; // extract operator without =
                    let hash = calc_fn_hash(empty(), op, 2, empty());
                    let args = &mut [
                        &mut self.eval_expr(scope, mods, state, lib, this_ptr, lhs_expr, level)?,
                        &mut rhs_val,
                    ];
                    self.exec_fn_call(state, lib, op, true, hash, args, false, false, None, level)
                        .map(|(v, _)| v)
                        .map_err(|err| err.new_position(*op_pos))?
                });

                match lhs_expr {
                    // name op= rhs
                    Expr::Variable(_) => unreachable!(),
                    // idx_lhs[idx_expr] op= rhs
                    #[cfg(not(feature = "no_index"))]
                    Expr::Index(_) => {
                        self.eval_dot_index_chain(
                            scope, mods, state, lib, this_ptr, lhs_expr, level, new_val,
                        )?;
                        Ok(Default::default())
                    }
                    // dot_lhs.dot_rhs op= rhs
                    #[cfg(not(feature = "no_object"))]
                    Expr::Dot(_) => {
                        self.eval_dot_index_chain(
                            scope, mods, state, lib, this_ptr, lhs_expr, level, new_val,
                        )?;
                        Ok(Default::default())
                    }
                    // Error assignment to constant
                    expr if expr.is_constant() => {
                        Err(Box::new(EvalAltResult::ErrorAssignmentToConstant(
                            expr.get_constant_str(),
                            expr.position(),
                        )))
                    }
                    // Syntax error
                    expr => Err(Box::new(EvalAltResult::ErrorAssignmentToUnknownLHS(
                        expr.position(),
                    ))),
                }
            }

            // lhs[idx_expr]
            #[cfg(not(feature = "no_index"))]
            Expr::Index(_) => {
                self.eval_dot_index_chain(scope, mods, state, lib, this_ptr, expr, level, None)
            }

            // lhs.dot_rhs
            #[cfg(not(feature = "no_object"))]
            Expr::Dot(_) => {
                self.eval_dot_index_chain(scope, mods, state, lib, this_ptr, expr, level, None)
            }

            #[cfg(not(feature = "no_index"))]
            Expr::Array(x) => Ok(Dynamic(Union::Array(Box::new(
                x.0.iter()
                    .map(|item| self.eval_expr(scope, mods, state, lib, this_ptr, item, level))
                    .collect::<Result<Vec<_>, _>>()?,
            )))),

            #[cfg(not(feature = "no_object"))]
            Expr::Map(x) => Ok(Dynamic(Union::Map(Box::new(
                x.0.iter()
                    .map(|((key, _), expr)| {
                        self.eval_expr(scope, mods, state, lib, this_ptr, expr, level)
                            .map(|val| (key.clone(), val))
                    })
                    .collect::<Result<HashMap<_, _>, _>>()?,
            )))),

            // Normal function call
            Expr::FnCall(x) if x.1.is_none() => {
                let ((name, native, pos), _, hash, args_expr, def_val) = x.as_ref();
                self.make_function_call(
                    scope, mods, state, lib, this_ptr, name, args_expr, *def_val, *hash, *native,
                    level,
                )
                .map_err(|err| err.new_position(*pos))
            }

            // Module-qualified function call
            Expr::FnCall(x) if x.1.is_some() => {
                let ((name, _, pos), modules, hash, args_expr, def_val) = x.as_ref();
                self.make_qualified_function_call(
                    scope, mods, state, lib, this_ptr, modules, name, args_expr, *def_val, *hash,
                    level,
                )
                .map_err(|err| err.new_position(*pos))
            }

            Expr::In(x) => self.eval_in_expr(scope, mods, state, lib, this_ptr, &x.0, &x.1, level),

            Expr::And(x) => {
                let (lhs, rhs, _) = x.as_ref();
                Ok((self
                    .eval_expr(scope, mods, state, lib, this_ptr, lhs, level)?
                    .as_bool()
                    .map_err(|_| {
                        EvalAltResult::ErrorBooleanArgMismatch("AND".into(), lhs.position())
                    })?
                    && // Short-circuit using &&
                self
                    .eval_expr(scope, mods, state, lib, this_ptr, rhs, level)?
                    .as_bool()
                    .map_err(|_| {
                        EvalAltResult::ErrorBooleanArgMismatch("AND".into(), rhs.position())
                    })?)
                .into())
            }

            Expr::Or(x) => {
                let (lhs, rhs, _) = x.as_ref();
                Ok((self
                    .eval_expr(scope, mods, state, lib, this_ptr, lhs, level)?
                    .as_bool()
                    .map_err(|_| {
                        EvalAltResult::ErrorBooleanArgMismatch("OR".into(), lhs.position())
                    })?
                    || // Short-circuit using ||
                self
                    .eval_expr(scope, mods, state, lib, this_ptr, rhs, level)?
                    .as_bool()
                    .map_err(|_| {
                        EvalAltResult::ErrorBooleanArgMismatch("OR".into(), rhs.position())
                    })?)
                .into())
            }

            Expr::True(_) => Ok(true.into()),
            Expr::False(_) => Ok(false.into()),
            Expr::Unit(_) => Ok(().into()),

            Expr::Custom(x) => {
                let func = (x.0).1.as_ref();
                let ep = (x.0).0.iter().map(|e| e.into()).collect::<StaticVec<_>>();
                let mut context = EvalContext {
                    mods,
                    state,
                    lib,
                    this_ptr,
                    level,
                };
                func(self, &mut context, scope, ep.as_ref())
            }

            _ => unreachable!(),
        };

        self.check_data_size(result)
            .map_err(|err| err.new_position(expr.position()))
    }

    /// Evaluate a statement
    pub(crate) fn eval_stmt(
        &self,
        scope: &mut Scope,
        mods: &mut Imports,
        state: &mut State,
        lib: &Module,
        this_ptr: &mut Option<&mut Dynamic>,
        stmt: &Stmt,
        level: usize,
    ) -> Result<Dynamic, Box<EvalAltResult>> {
        self.inc_operations(state)
            .map_err(|err| err.new_position(stmt.position()))?;

        let result = match stmt {
            // No-op
            Stmt::Noop(_) => Ok(Default::default()),

            // Expression as statement
            Stmt::Expr(expr) => self.eval_expr(scope, mods, state, lib, this_ptr, expr, level),

            // Block scope
            Stmt::Block(x) => {
                let prev_scope_len = scope.len();
                let prev_mods_len = mods.len();
                state.scope_level += 1;

                let result = x.0.iter().try_fold(Default::default(), |_, stmt| {
                    self.eval_stmt(scope, mods, state, lib, this_ptr, stmt, level)
                });

                scope.rewind(prev_scope_len);
                mods.truncate(prev_mods_len);
                state.scope_level -= 1;

                // The impact of an eval statement goes away at the end of a block
                // because any new variables introduced will go out of scope
                state.always_search = false;

                result
            }

            // If-else statement
            Stmt::IfThenElse(x) => {
                let (expr, if_block, else_block) = x.as_ref();

                self.eval_expr(scope, mods, state, lib, this_ptr, expr, level)?
                    .as_bool()
                    .map_err(|_| Box::new(EvalAltResult::ErrorLogicGuard(expr.position())))
                    .and_then(|guard_val| {
                        if guard_val {
                            self.eval_stmt(scope, mods, state, lib, this_ptr, if_block, level)
                        } else if let Some(stmt) = else_block {
                            self.eval_stmt(scope, mods, state, lib, this_ptr, stmt, level)
                        } else {
                            Ok(Default::default())
                        }
                    })
            }

            // While loop
            Stmt::While(x) => loop {
                let (expr, body) = x.as_ref();

                match self
                    .eval_expr(scope, mods, state, lib, this_ptr, expr, level)?
                    .as_bool()
                {
                    Ok(true) => {
                        match self.eval_stmt(scope, mods, state, lib, this_ptr, body, level) {
                            Ok(_) => (),
                            Err(err) => match *err {
                                EvalAltResult::ErrorLoopBreak(false, _) => (),
                                EvalAltResult::ErrorLoopBreak(true, _) => {
                                    return Ok(Default::default())
                                }
                                _ => return Err(err),
                            },
                        }
                    }
                    Ok(false) => return Ok(Default::default()),
                    Err(_) => {
                        return Err(Box::new(EvalAltResult::ErrorLogicGuard(expr.position())))
                    }
                }
            },

            // Loop statement
            Stmt::Loop(body) => loop {
                match self.eval_stmt(scope, mods, state, lib, this_ptr, body, level) {
                    Ok(_) => (),
                    Err(err) => match *err {
                        EvalAltResult::ErrorLoopBreak(false, _) => (),
                        EvalAltResult::ErrorLoopBreak(true, _) => return Ok(Default::default()),
                        _ => return Err(err),
                    },
                }
            },

            // For loop
            Stmt::For(x) => {
                let (name, expr, stmt) = x.as_ref();
                let iter_type = self.eval_expr(scope, mods, state, lib, this_ptr, expr, level)?;
                let tid = iter_type.type_id();

                if let Some(func) = self
                    .global_module
                    .get_iter(tid)
                    .or_else(|| self.packages.get_iter(tid))
                {
                    // Add the loop variable
                    let var_name = unsafe_cast_var_name_to_lifetime(name, &state);
                    scope.push(var_name, ());
                    let index = scope.len() - 1;
                    state.scope_level += 1;

                    for loop_var in func(iter_type) {
                        *scope.get_mut(index).0 = loop_var;
                        self.inc_operations(state)
                            .map_err(|err| err.new_position(stmt.position()))?;

                        match self.eval_stmt(scope, mods, state, lib, this_ptr, stmt, level) {
                            Ok(_) => (),
                            Err(err) => match *err {
                                EvalAltResult::ErrorLoopBreak(false, _) => (),
                                EvalAltResult::ErrorLoopBreak(true, _) => break,
                                _ => return Err(err),
                            },
                        }
                    }

                    scope.rewind(scope.len() - 1);
                    state.scope_level -= 1;
                    Ok(Default::default())
                } else {
                    Err(Box::new(EvalAltResult::ErrorFor(x.1.position())))
                }
            }

            // Continue statement
            Stmt::Continue(pos) => Err(Box::new(EvalAltResult::ErrorLoopBreak(false, *pos))),

            // Break statement
            Stmt::Break(pos) => Err(Box::new(EvalAltResult::ErrorLoopBreak(true, *pos))),

            // Return value
            Stmt::ReturnWithVal(x) if x.1.is_some() && (x.0).0 == ReturnType::Return => {
                Err(Box::new(EvalAltResult::Return(
                    self.eval_expr(
                        scope,
                        mods,
                        state,
                        lib,
                        this_ptr,
                        x.1.as_ref().unwrap(),
                        level,
                    )?,
                    (x.0).1,
                )))
            }

            // Empty return
            Stmt::ReturnWithVal(x) if (x.0).0 == ReturnType::Return => {
                Err(Box::new(EvalAltResult::Return(Default::default(), (x.0).1)))
            }

            // Throw value
            Stmt::ReturnWithVal(x) if x.1.is_some() && (x.0).0 == ReturnType::Exception => {
                let val = self.eval_expr(
                    scope,
                    mods,
                    state,
                    lib,
                    this_ptr,
                    x.1.as_ref().unwrap(),
                    level,
                )?;
                Err(Box::new(EvalAltResult::ErrorRuntime(
                    val.take_string().unwrap_or_else(|_| "".into()),
                    (x.0).1,
                )))
            }

            // Empty throw
            Stmt::ReturnWithVal(x) if (x.0).0 == ReturnType::Exception => {
                Err(Box::new(EvalAltResult::ErrorRuntime("".into(), (x.0).1)))
            }

            Stmt::ReturnWithVal(_) => unreachable!(),

            // Let statement
            Stmt::Let(x) if x.1.is_some() => {
                let ((var_name, _), expr) = x.as_ref();
                let val = self.eval_expr(
                    scope,
                    mods,
                    state,
                    lib,
                    this_ptr,
                    expr.as_ref().unwrap(),
                    level,
                )?;
                let var_name = unsafe_cast_var_name_to_lifetime(var_name, &state);
                scope.push_dynamic_value(var_name, ScopeEntryType::Normal, val, false);
                Ok(Default::default())
            }

            Stmt::Let(x) => {
                let ((var_name, _), _) = x.as_ref();
                let var_name = unsafe_cast_var_name_to_lifetime(var_name, &state);
                scope.push(var_name, ());
                Ok(Default::default())
            }

            // Const statement
            Stmt::Const(x) if x.1.is_constant() => {
                let ((var_name, _), expr) = x.as_ref();
                let val = self.eval_expr(scope, mods, state, lib, this_ptr, &expr, level)?;
                let var_name = unsafe_cast_var_name_to_lifetime(var_name, &state);
                scope.push_dynamic_value(var_name, ScopeEntryType::Constant, val, true);
                Ok(Default::default())
            }

            // Const expression not constant
            Stmt::Const(_) => unreachable!(),

            // Import statement
            Stmt::Import(x) => {
                let (expr, (name, pos)) = x.as_ref();

                // Guard against too many modules
                if state.modules >= self.max_modules {
                    return Err(Box::new(EvalAltResult::ErrorTooManyModules(*pos)));
                }

                if let Some(path) = self
                    .eval_expr(scope, mods, state, lib, this_ptr, &expr, level)?
                    .try_cast::<ImmutableString>()
                {
                    #[cfg(not(feature = "no_module"))]
                    if let Some(resolver) = &self.module_resolver {
                        let mut module = resolver.resolve(self, &path, expr.position())?;
                        module.index_all_sub_modules();
                        mods.push((name.clone().into(), module));

                        state.modules += 1;

                        Ok(Default::default())
                    } else {
                        Err(Box::new(EvalAltResult::ErrorModuleNotFound(
                            path.to_string(),
                            expr.position(),
                        )))
                    }

                    #[cfg(feature = "no_module")]
                    Ok(Default::default())
                } else {
                    Err(Box::new(EvalAltResult::ErrorImportExpr(expr.position())))
                }
            }

            // Export statement
            Stmt::Export(list) => {
                for ((id, id_pos), rename) in list.iter() {
                    // Mark scope variables as public
                    if let Some(index) = scope.get_index(id).map(|(i, _)| i) {
                        let alias = rename.as_ref().map(|(n, _)| n).unwrap_or_else(|| id);
                        scope.set_entry_alias(index, alias.clone());
                    } else {
                        return Err(Box::new(EvalAltResult::ErrorVariableNotFound(
                            id.into(),
                            *id_pos,
                        )));
                    }
                }
                Ok(Default::default())
            }
        };

        self.check_data_size(result)
            .map_err(|err| err.new_position(stmt.position()))
    }

    /// Check a result to ensure that the data size is within allowable limit.
    /// Position in `EvalAltResult` may be None and should be set afterwards.
    fn check_data_size(
        &self,
        result: Result<Dynamic, Box<EvalAltResult>>,
    ) -> Result<Dynamic, Box<EvalAltResult>> {
        #[cfg(feature = "unchecked")]
        return result;

        // If no data size limits, just return
        if self.max_string_size + self.max_array_size + self.max_map_size == 0 {
            return result;
        }

        // Recursively calculate the size of a value (especially `Array` and `Map`)
        fn calc_size(value: &Dynamic) -> (usize, usize, usize) {
            match value {
                #[cfg(not(feature = "no_index"))]
                Dynamic(Union::Array(arr)) => {
                    let mut arrays = 0;
                    let mut maps = 0;

                    arr.iter().for_each(|value| match value {
                        Dynamic(Union::Array(_)) => {
                            let (a, m, _) = calc_size(value);
                            arrays += a;
                            maps += m;
                        }
                        #[cfg(not(feature = "no_object"))]
                        Dynamic(Union::Map(_)) => {
                            let (a, m, _) = calc_size(value);
                            arrays += a;
                            maps += m;
                        }
                        _ => arrays += 1,
                    });

                    (arrays, maps, 0)
                }
                #[cfg(not(feature = "no_object"))]
                Dynamic(Union::Map(map)) => {
                    let mut arrays = 0;
                    let mut maps = 0;

                    map.values().for_each(|value| match value {
                        #[cfg(not(feature = "no_index"))]
                        Dynamic(Union::Array(_)) => {
                            let (a, m, _) = calc_size(value);
                            arrays += a;
                            maps += m;
                        }
                        Dynamic(Union::Map(_)) => {
                            let (a, m, _) = calc_size(value);
                            arrays += a;
                            maps += m;
                        }
                        _ => maps += 1,
                    });

                    (arrays, maps, 0)
                }
                Dynamic(Union::Str(s)) => (0, 0, s.len()),
                _ => (0, 0, 0),
            }
        }

        match result {
            // Simply return all errors
            Err(_) => return result,
            // String with limit
            Ok(Dynamic(Union::Str(_))) if self.max_string_size > 0 => (),
            // Array with limit
            #[cfg(not(feature = "no_index"))]
            Ok(Dynamic(Union::Array(_))) if self.max_array_size > 0 => (),
            // Map with limit
            #[cfg(not(feature = "no_object"))]
            Ok(Dynamic(Union::Map(_))) if self.max_map_size > 0 => (),
            // Everything else is simply returned
            Ok(_) => return result,
        };

        let (arr, map, s) = calc_size(result.as_ref().unwrap());

        if s > self.max_string_size {
            Err(Box::new(EvalAltResult::ErrorDataTooLarge(
                "Length of string".to_string(),
                self.max_string_size,
                s,
                Position::none(),
            )))
        } else if arr > self.max_array_size {
            Err(Box::new(EvalAltResult::ErrorDataTooLarge(
                "Size of array".to_string(),
                self.max_array_size,
                arr,
                Position::none(),
            )))
        } else if map > self.max_map_size {
            Err(Box::new(EvalAltResult::ErrorDataTooLarge(
                "Number of properties in object map".to_string(),
                self.max_map_size,
                map,
                Position::none(),
            )))
        } else {
            result
        }
    }

    /// Check if the number of operations stay within limit.
    /// Position in `EvalAltResult` is `None` and must be set afterwards.
    pub(crate) fn inc_operations(&self, state: &mut State) -> Result<(), Box<EvalAltResult>> {
        state.operations += 1;

        #[cfg(not(feature = "unchecked"))]
        // Guard against too many operations
        if self.max_operations > 0 && state.operations > self.max_operations {
            return Err(Box::new(EvalAltResult::ErrorTooManyOperations(
                Position::none(),
            )));
        }

        // Report progress - only in steps
        if let Some(progress) = &self.progress {
            if !progress(&state.operations) {
                // Terminate script if progress returns false
                return Err(Box::new(EvalAltResult::ErrorTerminated(Position::none())));
            }
        }

        Ok(())
    }

    /// Map a type_name into a pretty-print name
    pub(crate) fn map_type_name<'a>(&'a self, name: &'a str) -> &'a str {
        self.type_names
            .as_ref()
            .and_then(|t| t.get(name).map(String::as_str))
            .unwrap_or(map_std_type_name(name))
    }
}
