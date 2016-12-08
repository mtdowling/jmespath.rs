//! JMESPath functions.

use std::collections::HashMap;
use std::collections::BTreeMap;
use std::cmp::{max, min};
use std::fmt;

use {Context, Error, ErrorReason, Rcvar, RuntimeError};
use interpreter::{interpret, SearchResult};
use variable::{Variable, JmespathType};

/* ------------------------------------------
 * Argument types
 * ------------------------------------------ */

/// Function argument types used when validating.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ArgumentType {
    Any,
    Null,
    String,
    Number,
    Bool,
    Object,
    Array,
    Expref,
    /// Each element of the array must matched the provided type.
    TypedArray(Box<ArgumentType>),
    /// Accepts one of a number of `ArgumentType`s
    Union(Vec<ArgumentType>),
}

impl ArgumentType {
    /// Returns true/false if the variable is valid for the type.
    pub fn is_valid(&self, value: &Rcvar) -> bool {
        use self::ArgumentType::*;
        match *self {
            Any => true,
            Null if value.is_null() => true,
            String if value.is_string() => true,
            Number if value.is_number() => true,
            Object if value.is_object() => true,
            Bool if value.is_boolean() => true,
            Expref if value.is_expref() => true,
            Array if value.is_array() => true,
            TypedArray(ref t) if value.is_array() => {
                value.as_array().unwrap().iter().all(|v| t.is_valid(v))
            },
            Union(ref types) => types.iter().any(|t| t.is_valid(value)),
            _ => false
        }
    }
}

impl fmt::Display for ArgumentType {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        use self::ArgumentType::*;
        match *self {
            Any => write!(fmt, "any"),
            String => write!(fmt, "string"),
            Number => write!(fmt, "number"),
            Bool => write!(fmt, "boolean"),
            Array => write!(fmt, "array"),
            Object => write!(fmt, "object"),
            Null => write!(fmt, "null"),
            Expref => write!(fmt, "expref"),
            TypedArray(ref t) => write!(fmt, "array[{}]", t),
            Union(ref types) => {
                let str_value = types.iter().map(|t| t.to_string()).collect::<Vec<_>>().join("|");
                write!(fmt, "{}", str_value)
            },
        }
    }
}

macro_rules! arg {
    (any) => (ArgumentType::Any);
    (null) => (ArgumentType::Null);
    (string) => (ArgumentType::String);
    (bool) => (ArgumentType::Bool);
    (number) => (ArgumentType::Number);
    (object) => (ArgumentType::Object);
    (expref) => (ArgumentType::Expref);
    (array_number) => (ArgumentType::TypedArray(Box::new(ArgumentType::Number)));
    (array_string) => (ArgumentType::TypedArray(Box::new(ArgumentType::String)));
    (array) => (ArgumentType::Array);
    ($($x:ident) | *) => (ArgumentType::Union(vec![$(arg!($x)), *]));
}

/* ------------------------------------------
 * Function registry and default registry
 * ------------------------------------------ */

/// Stores and evaluates JMESPath functions.
pub struct FnRegistry {
    functions: HashMap<String, Box<Function>>,
}

impl FnRegistry {
    /// Creates a new, empty function registry.
    pub fn new() -> FnRegistry {
        FnRegistry {
            functions: HashMap::with_capacity(26),
        }
    }

    /// Creates a new registry that uses the default JMESPath functions.
    pub fn from_defaults() -> FnRegistry {
        let mut registry = FnRegistry::new();
        registry.functions.insert("abs".to_owned(), Box::new(AbsFn::new()));
        registry.functions.insert("avg".to_owned(), Box::new(AvgFn::new()));
        registry.functions.insert("ceil".to_owned(), Box::new(CeilFn::new()));
        registry.functions.insert("contains".to_owned(), Box::new(ContainsFn::new()));
        registry.functions.insert("ends_with".to_owned(), Box::new(EndsWithFn::new()));
        registry.functions.insert("floor".to_owned(), Box::new(FloorFn::new()));
        registry.functions.insert("join".to_owned(), Box::new(JoinFn::new()));
        registry.functions.insert("keys".to_owned(), Box::new(KeysFn::new()));
        registry.functions.insert("length".to_owned(), Box::new(LengthFn::new()));
        registry.functions.insert("map".to_owned(), Box::new(MapFn::new()));
        registry.functions.insert("min".to_owned(), Box::new(MinFn::new()));
        registry.functions.insert("max".to_owned(), Box::new(MaxFn::new()));
        registry.functions.insert("max_by".to_owned(), Box::new(MaxByFn::new()));
        registry.functions.insert("min_by".to_owned(), Box::new(MinByFn::new()));
        registry.functions.insert("merge".to_owned(), Box::new(MergeFn::new()));
        registry.functions.insert("not_null".to_owned(), Box::new(NotNullFn::new()));
        registry.functions.insert("reverse".to_owned(), Box::new(ReverseFn::new()));
        registry.functions.insert("sort".to_owned(), Box::new(SortFn::new()));
        registry.functions.insert("sort_by".to_owned(), Box::new(SortByFn::new()));
        registry.functions.insert("starts_with".to_owned(), Box::new(StartsWithFn::new()));
        registry.functions.insert("sum".to_owned(), Box::new(SumFn::new()));
        registry.functions.insert("to_array".to_owned(), Box::new(ToArrayFn::new()));
        registry.functions.insert("to_number".to_owned(), Box::new(ToNumberFn::new()));
        registry.functions.insert("to_string".to_owned(), Box::new(ToStringFn::new()));
        registry.functions.insert("type".to_owned(), Box::new(TypeFn::new()));
        registry.functions.insert("values".to_owned(), Box::new(ValuesFn::new()));
        registry
    }

    /// Adds a new custom function to the registry.
    pub fn register_function(&mut self, name: &str, f: Box<Function>) {
        self.functions.insert(name.to_owned(), f);
    }

    /// Deregisters a function by name and returns it if found.
    pub fn deregister_function(&mut self, name: &str) -> Option<Box<Function>> {
        self.functions.remove(name)
    }

    /// Evaluates a function by name in the registry.
    ///
    /// The registry is responsible for validating function signatures
    /// before invoking the function, and functions should assume that
    /// the arguments provided to them are correct.
    pub fn evaluate(&self, name: &str, args: &[Rcvar], ctx: &mut Context) -> SearchResult {
        match self.functions.get(name) {
            Some(f) => f.evaluate(args, ctx),
            None => {
                Err(Error::from_ctx(ctx, ErrorReason::Runtime(
                    RuntimeError::UnknownFunction(name.to_owned())
                )))
            }
        }
    }
}

/* ------------------------------------------
 * Custom functions.
 * ------------------------------------------ */

/// Custom function that allows the creation of runtime functions.
pub struct CustomFunction {
    signature: Signature,
    f: Box<(Fn(&[Rcvar], &mut Context) -> SearchResult) + Sync>,
}

impl CustomFunction {
    /// Creates a new custom function.
    pub fn new(fn_signature: Signature,
               f: Box<(Fn(&[Rcvar], &mut Context) -> SearchResult) + Sync>)
               -> CustomFunction {
        CustomFunction {
            signature: fn_signature,
            f: f,
        }
    }
}

impl Function for CustomFunction {
    fn evaluate(&self, args: &[Rcvar], ctx: &mut Context) -> SearchResult {
        try!(self.signature.validate(args, ctx));
        (self.f)(args, ctx)
    }
}

/* ------------------------------------------
 * Function and signature types
 * ------------------------------------------ */

/// Represents a function's signature.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Signature {
    pub inputs: Vec<ArgumentType>,
    pub variadic: Option<ArgumentType>,
    pub output: ArgumentType,
}

impl Signature {
    /// Creates a new Signature struct.
    pub fn new(inputs: Vec<ArgumentType>,
               variadic: Option<ArgumentType>,
               output: ArgumentType)
               -> Signature {
        Signature {
            inputs: inputs,
            variadic: variadic,
            output: output,
        }
    }

    /// Validates the arity of a function. If the arity is invalid, a runtime
    /// error is returned with the relative position of the error and the
    /// expression that was being executed.
    pub fn validate_arity(&self, actual: usize, ctx: &Context) -> Result<(), Error> {
        let expected = self.inputs.len();
        if self.variadic.is_some() {
            if actual >= expected {
                Ok(())
            } else {
                Err(Error::from_ctx(ctx, ErrorReason::Runtime(RuntimeError::NotEnoughArguments {
                    expected: expected,
                    actual: actual
                })))
            }
        } else if actual == expected {
            Ok(())
        } else if actual < expected {
            Err(Error::from_ctx(ctx, ErrorReason::Runtime(RuntimeError::NotEnoughArguments {
                expected: expected,
                actual: actual,
            })))
        } else {
            Err(Error::from_ctx(ctx, ErrorReason::Runtime(RuntimeError::TooManyArguments {
                expected: expected,
                actual: actual,
            })))
        }
    }

    /// Validates the provided function arguments against the signature.
    pub fn validate(&self, args: &[Rcvar], ctx: &Context) -> Result<(), Error> {
        try!(self.validate_arity(args.len(), ctx));
        if let Some(ref variadic) = self.variadic {
            for (k, v) in args.iter().enumerate() {
                let validator = self.inputs.get(k).unwrap_or(variadic);
                try!(self.validate_arg(ctx, k, v, validator));
            }
        } else {
            for (k, v) in args.iter().enumerate() {
                try!(self.validate_arg(ctx, k, v, &self.inputs[k]));
            }
        }
        Ok(())
    }

    fn validate_arg(&self,
                    ctx: &Context,
                    position: usize,
                    value: &Rcvar,
                    validator: &ArgumentType)
                    -> Result<(), Error> {
        if validator.is_valid(value) {
            Ok(())
        } else {
            Err(Error::from_ctx(ctx, ErrorReason::Runtime(RuntimeError::InvalidType {
                expected: validator.to_string(),
                actual: value.get_type().to_string(),
                position: position
            })))
        }
    }
}

/// Represents a JMESPath function.
pub trait Function: Sync {
    /// Evaluates the function against an in-memory variable.
    fn evaluate(&self, args: &[Rcvar], ctx: &mut Context) -> SearchResult;
}

/* ------------------------------------------
 * Function definitions
 * ------------------------------------------ */

/// Macro to more easily and quickly define a function and signature.
macro_rules! defn {
    ($name:ident, $args:expr, $variadic:expr, $retval:expr) => {
        struct $name {
            signature: Signature,
        }

        impl $name {
            pub fn new() -> $name {
                $name {
                    signature: Signature::new($args, $variadic, $retval),
                }
            }
        }
    };
}

/// Macro used to implement max_by and min_by functions.
macro_rules! min_and_max_by {
    ($ctx:expr, $operator:ident, $args:expr) => (
        {
            let vals = $args[0].as_array().unwrap();
            // Return null when there are not values in the array
            if vals.is_empty() {
                return Ok(Rcvar::new(Variable::Null));
            }
            let ast = $args[1].as_expref().unwrap();
            // Map over the first value to get the homogeneous required return type
            let initial = try!(interpret(&vals[0], &ast, $ctx));
            let entered_type = initial.get_type();
            if entered_type != JmespathType::String && entered_type != JmespathType::Number {
                return Err(Error::from_ctx($ctx,
                    ErrorReason::Runtime(RuntimeError::InvalidReturnType {
                        expected: "expression->number|expression->string".to_owned(),
                        actual: entered_type.to_string(),
                        position: 1,
                        invocation: 1
                    }
                )));
            }
            // Map over each value, finding the best candidate value and fail on error.
            let mut candidate = (vals[0].clone(), initial.clone());
            for (invocation, v) in vals.iter().enumerate().skip(1) {
                let mapped = try!(interpret(v, &ast, $ctx));
                if mapped.get_type() != entered_type {
                    return Err(Error::from_ctx($ctx,
                        ErrorReason::Runtime(RuntimeError::InvalidReturnType {
                            expected: format!("expression->{}", entered_type),
                            actual: mapped.get_type().to_string(),
                            position: 1,
                            invocation: invocation
                        }
                    )));
                }
                if mapped.$operator(&candidate.1) {
                    candidate = (v.clone(), mapped);
                }
            }
            Ok(candidate.0)
        }
    )
}

/// Macro used to implement max and min functions.
macro_rules! min_and_max {
    ($operator:ident, $args:expr) => (
        {
            let values = $args[0].as_array().unwrap();
            if values.is_empty() {
                Ok(Rcvar::new(Variable::Null))
            } else {
                let result: Rcvar = values
                    .iter()
                    .skip(1)
                    .fold(values[0].clone(), |acc, item| $operator(acc, item.clone()));
                Ok(result)
            }
        }
    )
}

defn!(AbsFn, vec![arg!(number)], None, arg!(number));

impl Function for AbsFn {
    fn evaluate(&self, args: &[Rcvar], ctx: &mut Context) -> SearchResult {
        try!(self.signature.validate(args, ctx));
        match *args[0] {
            Variable::Number(n) => Ok(Rcvar::new(Variable::Number(n.abs()))),
            _ => Ok(args[0].clone())
        }
    }
}

defn!(AvgFn, vec![arg!(array_number)], None, arg!(number));

impl Function for AvgFn {
    fn evaluate(&self, args: &[Rcvar], ctx: &mut Context) -> SearchResult {
        try!(self.signature.validate(args, ctx));
        let values = args[0].as_array().unwrap();
        let sum = values.iter()
            .map(|n| n.as_number().unwrap())
            .fold(0f64, |a, ref b| a + b);
        Ok(Rcvar::new(Variable::Number(sum / (values.len() as f64))))
    }
}

defn!(CeilFn, vec![arg!(number)], None, arg!(number));

impl Function for CeilFn {
    fn evaluate(&self, args: &[Rcvar], ctx: &mut Context) -> SearchResult {
        try!(self.signature.validate(args, ctx));
        let n = args[0].as_number().unwrap();
        Ok(Rcvar::new(Variable::Number(n.ceil())))
    }
}

defn!(ContainsFn, vec![arg!(string | array), arg!(any)], None, arg!(bool));

impl Function for ContainsFn {
    fn evaluate(&self, args: &[Rcvar], ctx: &mut Context) -> SearchResult {
        try!(self.signature.validate(args, ctx));
        let haystack = &args[0];
        let needle = &args[1];
        match **haystack {
           Variable::Array(ref a) => {
               Ok(Rcvar::new(Variable::Bool(a.contains(&needle))))
           },
           Variable::String(ref subj) => {
               match needle.as_string() {
                   None => Ok(Rcvar::new(Variable::Bool(false))),
                   Some(s) => Ok(Rcvar::new(Variable::Bool(subj.contains(s))))
               }
           },
           _ => unreachable!()
        }
    }
}

defn!(EndsWithFn, vec![arg!(string), arg!(string)], None, arg!(bool));

impl Function for EndsWithFn {
    fn evaluate(&self, args: &[Rcvar], ctx: &mut Context) -> SearchResult {
        try!(self.signature.validate(args, ctx));
        let subject = args[0].as_string().unwrap();
        let search = args[1].as_string().unwrap();
        Ok(Rcvar::new(Variable::Bool(subject.ends_with(search))))
    }
}

defn!(FloorFn, vec![arg!(number)], None, arg!(number));

impl Function for FloorFn {
    fn evaluate(&self, args: &[Rcvar], ctx: &mut Context) -> SearchResult {
        try!(self.signature.validate(args, ctx));
        let n = args[0].as_number().unwrap();
        Ok(Rcvar::new(Variable::Number(n.floor())))
    }
}

defn!(JoinFn, vec![arg!(string), arg!(array_string)], None, arg!(string));

impl Function for JoinFn {
    fn evaluate(&self, args: &[Rcvar], ctx: &mut Context) -> SearchResult {
        try!(self.signature.validate(args, ctx));
        let glue = args[0].as_string().unwrap();
        let values = args[1].as_array().unwrap();
        let result = values.iter()
            .map(|v| v.as_string().unwrap())
            .cloned()
            .collect::<Vec<String>>()
            .join(&glue);
        Ok(Rcvar::new(Variable::String(result)))
    }
}

defn!(KeysFn, vec![arg!(object)], None, arg!(array));

impl Function for KeysFn {
    fn evaluate(&self, args: &[Rcvar], ctx: &mut Context) -> SearchResult {
        try!(self.signature.validate(args, ctx));
        let object = args[0].as_object().unwrap();
        let keys = object.keys()
            .map(|k| Rcvar::new(Variable::String((*k).clone())))
            .collect::<Vec<Rcvar>>();
        Ok(Rcvar::new(Variable::Array(keys)))
    }
}

defn!(LengthFn, vec![arg!(array | object | string)], None, arg!(number));

impl Function for LengthFn {
    fn evaluate(&self, args: &[Rcvar], ctx: &mut Context) -> SearchResult {
        try!(self.signature.validate(args, ctx));
        match *args[0] {
            Variable::Array(ref a) => Ok(Rcvar::new(Variable::Number(a.len() as f64))),
            Variable::Object(ref m) => Ok(Rcvar::new(Variable::Number(m.len() as f64))),
            // Note that we need to count the code points not the number of unicode characters
            Variable::String(ref s) => Ok(Rcvar::new(Variable::Number(s.chars().count() as f64))),
            _ => unreachable!()
        }
    }
}

defn!(MapFn, vec![arg!(expref), arg!(array)], None, arg!(array));

impl Function for MapFn {
    fn evaluate(&self, args: &[Rcvar], ctx: &mut Context) -> SearchResult {
        try!(self.signature.validate(args, ctx));
        let ast = args[0].as_expref().unwrap();
        let values = args[1].as_array().unwrap();
        let mut results = vec![];
        for value in values {
            results.push(try!(interpret(&value, &ast, ctx)));
        }
        Ok(Rcvar::new(Variable::Array(results)))
    }
}

defn!(MaxFn, vec![arg!(array_string | array_number)], None, arg!(string | number));

impl Function for MaxFn {
    fn evaluate(&self, args: &[Rcvar], ctx: &mut Context) -> SearchResult {
        try!(self.signature.validate(args, ctx));
        min_and_max!(max, args)
    }
}

defn!(MinFn, vec![arg!(array_string | array_number)], None, arg!(string | number));

impl Function for MinFn {
    fn evaluate(&self, args: &[Rcvar], ctx: &mut Context) -> SearchResult {
        try!(self.signature.validate(args, ctx));
        min_and_max!(min, args)
    }
}

defn!(MaxByFn, vec![arg!(array), arg!(expref)], None, arg!(string | number));

impl Function for MaxByFn {
    fn evaluate(&self, args: &[Rcvar], ctx: &mut Context) -> SearchResult {
        try!(self.signature.validate(args, ctx));
        min_and_max_by!(ctx, gt, args)
    }
}

defn!(MinByFn, vec![arg!(array), arg!(expref)], None, arg!(string | number));

impl Function for MinByFn {
    fn evaluate(&self, args: &[Rcvar], ctx: &mut Context) -> SearchResult {
        try!(self.signature.validate(args, ctx));
        min_and_max_by!(ctx, lt, args)
    }
}

defn!(MergeFn, vec![arg!(object)], Some(arg!(object)), arg!(object));

impl Function for MergeFn {
    fn evaluate(&self, args: &[Rcvar], ctx: &mut Context) -> SearchResult {
        try!(self.signature.validate(args, ctx));
        let mut result = BTreeMap::new();
        for arg in args {
            result.extend(arg.as_object().unwrap().clone());
        }
        Ok(Rcvar::new(Variable::Object(result)))
    }
}

defn!(NotNullFn, vec![arg!(any)], Some(arg!(any)), arg!(any));

impl Function for NotNullFn {
    fn evaluate(&self, args: &[Rcvar], ctx: &mut Context) -> SearchResult {
        try!(self.signature.validate(args, ctx));
        for arg in args {
            if !arg.is_null() {
                return Ok(arg.clone());
            }
        }
        Ok(Rcvar::new(Variable::Null))
    }
}

defn!(ReverseFn, vec![arg!(array | string)], None, arg!(array | string));

impl Function for ReverseFn {
    fn evaluate(&self, args: &[Rcvar], ctx: &mut Context) -> SearchResult {
        try!(self.signature.validate(args, ctx));
        if args[0].is_array() {
            let mut values = args[0].as_array().unwrap().clone();
            values.reverse();
            Ok(Rcvar::new(Variable::Array(values)))
        } else {
            let word: String = args[0].as_string().unwrap().chars().rev().collect();
            Ok(Rcvar::new(Variable::String(word)))
        }
    }
}

defn!(SortFn, vec![arg!(array_string | array_number)], None, arg!(array));

impl Function for SortFn {
    fn evaluate(&self, args: &[Rcvar], ctx: &mut Context) -> SearchResult {
        try!(self.signature.validate(args, ctx));
        let mut values = args[0].as_array().unwrap().clone();
        values.sort();
        Ok(Rcvar::new(Variable::Array(values)))
    }
}

defn!(SortByFn, vec![arg!(array), arg!(expref)], None, arg!(array));

impl Function for SortByFn {
    fn evaluate(&self, args: &[Rcvar], ctx: &mut Context) -> SearchResult {
        try!(self.signature.validate(args, ctx));
        let vals = args[0].as_array().unwrap().clone();
        if vals.is_empty() {
            return Ok(Rcvar::new(Variable::Array(vals)));
        }
        let ast = args[1].as_expref().unwrap();
        let mut mapped: Vec<(Rcvar, Rcvar)> = vec![];
        let first_value = try!(interpret(&vals[0], &ast, ctx));
        let first_type = first_value.get_type();
        if first_type != JmespathType::String && first_type != JmespathType::Number {
            return Err(Error::from_ctx(ctx, ErrorReason::Runtime(RuntimeError::InvalidReturnType {
                expected: "expression->string|expression->number".to_owned(),
                actual: first_type.to_string(),
                position: 1,
                invocation: 1
            })));
        }
        mapped.push((vals[0].clone(), first_value.clone()));
        for (invocation, v) in vals.iter().enumerate().skip(1) {
            let mapped_value = try!(interpret(v, &ast, ctx));
            if mapped_value.get_type() != first_type {
                return Err(Error::from_ctx(ctx,
                    ErrorReason::Runtime(RuntimeError::InvalidReturnType {
                        expected: format!("expression->{}", first_type),
                        actual: mapped_value.get_type().to_string(),
                        position: 1,
                        invocation: invocation
                    }
                )));
            }
            mapped.push((v.clone(), mapped_value));
        }
        mapped.sort_by(|a, b| a.1.cmp(&b.1));
        let result = mapped.iter().map(|tuple| tuple.0.clone()).collect();
        Ok(Rcvar::new(Variable::Array(result)))
    }
}

defn!(StartsWithFn, vec![arg!(string), arg!(string)], None, arg!(bool));

impl Function for StartsWithFn {
    fn evaluate(&self, args: &[Rcvar], ctx: &mut Context) -> SearchResult {
        try!(self.signature.validate(args, ctx));
        let subject = args[0].as_string().unwrap();
        let search = args[1].as_string().unwrap();
        Ok(Rcvar::new(Variable::Bool(subject.starts_with(search))))
    }
}

defn!(SumFn, vec![arg!(array_number)], None, arg!(number));

impl Function for SumFn {
    fn evaluate(&self, args: &[Rcvar], ctx: &mut Context) -> SearchResult {
        try!(self.signature.validate(args, ctx));
        let result = args[0].as_array().unwrap().iter().fold(
            0.0, |acc, item| acc + item.as_number().unwrap());
        Ok(Rcvar::new(Variable::Number(result)))
    }
}

defn!(ToArrayFn, vec![arg!(any)], None, arg!(array));

impl Function for ToArrayFn {
    fn evaluate(&self, args: &[Rcvar], ctx: &mut Context) -> SearchResult {
        try!(self.signature.validate(args, ctx));
        match *args[0] {
            Variable::Array(_) => Ok(args[0].clone()),
            _ => Ok(Rcvar::new(Variable::Array(vec![args[0].clone()])))
        }
    }
}

defn!(ToNumberFn, vec![arg!(any)], None, arg!(number));

impl Function for ToNumberFn {
    fn evaluate(&self, args: &[Rcvar], ctx: &mut Context) -> SearchResult {
        try!(self.signature.validate(args, ctx));
        match *args[0] {
            Variable::Number(_) => Ok(args[0].clone()),
            Variable::String(ref s) => {
                match Variable::from_json(s) {
                    Ok(f)  => Ok(Rcvar::new(f)),
                    Err(_) => Ok(Rcvar::new(Variable::Null))
                }
            },
            _ => Ok(Rcvar::new(Variable::Null))
        }
    }
}

defn!(ToStringFn, vec![arg!(object | array | bool | number | string | null)], None, arg!(string));

impl Function for ToStringFn {
    fn evaluate(&self, args: &[Rcvar], ctx: &mut Context) -> SearchResult {
        try!(self.signature.validate(args, ctx));
        match *args[0] {
            Variable::String(_) => Ok(args[0].clone()),
            _ => Ok(Rcvar::new(Variable::String(args[0].to_string())))
        }
    }
}

defn!(TypeFn, vec![arg!(any)], None, arg!(string));

impl Function for TypeFn {
    fn evaluate(&self, args: &[Rcvar], ctx: &mut Context) -> SearchResult {
        try!(self.signature.validate(args, ctx));
        Ok(Rcvar::new(Variable::String(args[0].get_type().to_string())))
    }
}

defn!(ValuesFn, vec![arg!(object)], None, arg!(array));

impl Function for ValuesFn {
    fn evaluate(&self, args: &[Rcvar], ctx: &mut Context) -> SearchResult {
        try!(self.signature.validate(args, ctx));
        let map = args[0].as_object().unwrap();
        Ok(Rcvar::new(Variable::Array(map.values().cloned().collect::<Vec<Rcvar>>())))
    }
}
