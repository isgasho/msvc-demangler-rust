//===- MicrosoftDemangle.cpp ----------------------------------------------===//
//
//                     The LLVM Compiler Infrastructure
//
// This file is dual licensed under the MIT and the University of Illinois Open
// Source Licenses. See LICENSE.TXT for details.
//
//===----------------------------------------------------------------------===//
//
// This file defines a demangler for MSVC-style mangled symbols.
//
// This file has no dependencies on the rest of LLVM so that it can be
// easily reused in other programs such as libcxxabi.
//
//===----------------------------------------------------------------------===//

#[macro_use]
extern crate bitflags;

use std::env;
use std::io::Write;
use std::result;
use std::str;

#[derive(Debug, Clone, PartialEq)]
struct Error {
    s: String,
}

impl Error {
    fn new(s: String) -> Error {
        Error { s }
    }
}

impl From<std::str::Utf8Error> for Error {
    fn from(t: std::str::Utf8Error) -> Error {
        Error {
            s: format!("{:?}", t),
        }
    }
}
impl From<std::string::FromUtf8Error> for Error {
    fn from(t: std::string::FromUtf8Error) -> Error {
        Error {
            s: format!("{:?}", t),
        }
    }
}

#[derive(Debug, Clone)]
struct SerializeError {
    s: String,
}

impl From<std::str::Utf8Error> for SerializeError {
    fn from(err: std::str::Utf8Error) -> SerializeError {
        SerializeError {
            s: format!("{:?}", err),
        }
    }
}

impl From<std::io::Error> for SerializeError {
    fn from(err: std::io::Error) -> SerializeError {
        SerializeError {
            s: format!("{:?}", err),
        }
    }
}

type SerializeResult<T> = result::Result<T, SerializeError>;

type Result<T> = result::Result<T, Error>;

bitflags! {
    struct StorageClass: u32 {
        const CONST      = 0b00000001;
        const VOLATILE   = 0b00000010;
        const FAR        = 0b00000100;
        const HUGE       = 0b00001000;
        const UNALIGNED  = 0b00010000;
        const RESTRICT   = 0b00100000;
    }
}

// Calling conventions
enum CallingConv {
    Cdecl,
    Pascal,
    Thiscall,
    Stdcall,
    Fastcall,
    _Regcall,
}

bitflags! {
    struct FuncClass: u32 {
        const PUBLIC     = 0b00000001;
        const PROTECTED  = 0b00000010;
        const PRIVATE    = 0b00000100;
        const GLOBAL     = 0b00001000;
        const STATIC     = 0b00010000;
        const VIRTUAL    = 0b00100000;
        const FAR        = 0b01000000;
    }
}

// Represents an identifier which may be a template.
#[derive(Clone, Debug)]
struct Name<'a> {
    // Name read from an input string.
    name_str: &'a [u8],

    // Overloaded operators are represented as special names in mangled symbols.
    // If this is an operator name, "op" has an operator name (e.g. ">>").
    // Otherwise, empty.
    op: Option<&'static str>,

    // Template parameters. None if not a template.
    template_params: Option<Params<'a>>,
}

#[derive(Clone, Debug)]
struct NameSequence<'a> {
    names: Vec<Name<'a>>,
}

#[derive(Clone, Debug)]
struct Params<'a> {
    types: Vec<Type<'a>>,
}

impl<'a> Params<'a> {
    fn empty() -> Params<'a> {
        Params { types: Vec::new() }
    }
}

// The type class. Mangled symbols are first parsed and converted to
// this type and then converted to string.
#[derive(Clone, Debug)]
enum Type<'a> {
    None,
    MemberFunction(Params<'a>, StorageClass, Box<Type<'a>>),
    NonMemberFunction(Params<'a>, StorageClass, Box<Type<'a>>),
    Ptr(Box<Type<'a>>, StorageClass),
    Ref(Box<Type<'a>>, StorageClass),
    Array(i32, Box<Type<'a>>, StorageClass),

    Struct(NameSequence<'a>, StorageClass),
    Union(NameSequence<'a>, StorageClass),
    Class(NameSequence<'a>, StorageClass),
    Enum(NameSequence<'a>, StorageClass),

    Void(StorageClass),
    Bool(StorageClass),
    Char(StorageClass),
    Schar(StorageClass),
    Uchar(StorageClass),
    Short(StorageClass),
    Ushort(StorageClass),
    Int(StorageClass),
    Uint(StorageClass),
    Long(StorageClass),
    Ulong(StorageClass),
    Int64(StorageClass),
    Uint64(StorageClass),
    Wchar(StorageClass),
    Float(StorageClass),
    Double(StorageClass),
    Ldouble(StorageClass),
}

struct ParseResult<'a> {
    symbol: NameSequence<'a>,
    symbol_type: Type<'a>,
}

// Demangler class takes the main role in demangling symbols.
// It has a set of functions to parse mangled symbols into Type instnaces.
// It also has a set of functions to cnovert Type instances to strings.
struct ParserState<'a> {
    // Mangled symbol. read_* functions shorten this string
    // as they parse it.
    input: &'a [u8],

    // The first 10 names in a mangled name can be back-referenced by
    // special name @[0-9]. This is a storage for the first 10 names.
    memorized_names: Vec<&'a [u8]>,
}

impl<'a> ParserState<'a> {
    fn parse(mut self) -> Result<ParseResult<'a>> {
        // MSVC-style mangled symbols must start with b'?'.
        if !self.consume(b"?") {
            return Err(Error::new("does not start with b'?'".to_owned()));
        }

        // What follows is a main symbol name. This may include
        // namespaces or class names.
        let symbol = self.read_name()?;

        let symbol_type = if self.consume(b"3") {
            // Read a variable.
            self.read_var_type(StorageClass::empty())?
        } else if self.consume(b"Y") {
            // Read a non-member function.
            let _ = self.read_calling_conv()?;
            let storage_class = self.read_storage_class_for_return()?;
            let return_type = self.read_var_type(storage_class)?;
            let params = self.read_params()?;
            Type::NonMemberFunction(
                params.unwrap_or(Params::empty()),
                StorageClass::empty(),
                Box::new(return_type),
            )
        } else {
            // Read a member function.
            let _func_lass = self.read_func_class()?;
            let _is_64bit_ptr = self.expect(b"E");
            let access_class = self.read_func_access_class();
            let _calling_conv = self.read_calling_conv()?;
            let storage_class_for_return = self.read_storage_class_for_return()?;
            let return_type = self.read_func_return_type(storage_class_for_return)?;
            let params = self.read_params()?;
            Type::MemberFunction(
                params.unwrap_or(Params::empty()),
                access_class,
                Box::new(return_type),
            )
        };
        Ok(ParseResult {
            symbol,
            symbol_type,
        })
    }

    fn peek(&self) -> Option<u8> {
        self.input.first().map(|&u| u)
    }

    fn get(&mut self) -> Result<u8> {
        match self.peek() {
            Some(first) => {
                self.trim(1);
                Ok(first)
            }
            None => Err(Error::new("unexpected end of input".to_owned())),
        }
    }

    fn consume(&mut self, s: &[u8]) -> bool {
        if self.input.starts_with(s) {
            self.trim(s.len());
            true
        } else {
            false
        }
    }

    fn trim(&mut self, len: usize) {
        self.input = &self.input[len..]
    }

    fn expect(&mut self, s: &[u8]) -> Result<()> {
        if !self.consume(s) {
            return Err(Error::new(format!(
                "{} expected, but got {}",
                str::from_utf8(s)?,
                str::from_utf8(self.input)?
            )));
        }
        Ok(())
    }

    fn consume_digit(&mut self) -> Option<u8> {
        match self.peek() {
            Some(first) => {
                if char::from(first).is_digit(10) {
                    self.trim(1);
                    Some(first - b'0')
                } else {
                    None
                }
            }
            None => None,
        }
    }

    // Sometimes numbers are encoded in mangled symbols. For example,
    // "int (*x)[20]" is a valid C type (x is a pointer to an array of
    // length 20), so we need some way to embed numbers as part of symbols.
    // This function parses it.
    //
    // <number>               ::= [?] <non-negative integer>
    //
    // <non-negative integer> ::= <decimal digit> # when 1 <= Number <= 10
    //                        ::= <hex digit>+ @  # when Numbrer == 0 or >= 10
    //
    // <hex-digit>            ::= [A-P]           # A = 0, B = 1, ...
    fn read_number(&mut self) -> Result<i32> {
        let neg = self.consume(b"?");

        if let Some(digit) = self.consume_digit() {
            let ret = digit + 1;
            return Ok(if neg { -(ret as i32) } else { ret as i32 });
        }

        let orig = self.input;
        let mut i = 0;
        let mut ret = 0;
        for c in self.input {
            match *c {
                b'@' => {
                    self.trim(i + 1);
                    return Ok(if neg { -(ret as i32) } else { ret as i32 });
                }
                b'A'...b'P' => {
                    ret = (ret << 4) + ((c - b'A') as i32);
                    i += 1;
                }
                _ => {
                    return Err(Error::new(format!("bad number: {}", str::from_utf8(orig)?)));
                }
            }
        }
        Err(Error::new(format!("bad number: {}", str::from_utf8(orig)?)))
    }

    // Read until the next b'@'.
    fn read_string(&mut self) -> Result<&'a [u8]> {
        if let Some(pos) = self.input.iter().position(|&x| x == b'@') {
            let ret = &self.input[0..pos];
            self.trim(pos + 1);
            Ok(ret)
        } else {
            let error = format!("read_string: missing b'@': {}", str::from_utf8(self.input)?);
            Err(Error::new(error))
        }
    }

    // First 10 strings can be referenced by special names ?0, ?1, ..., ?9.
    // Memorize it.
    fn memorize_string(&mut self, s: &'a [u8]) {
        if self.memorized_names.len() < 10 && !self.memorized_names.contains(&s) {
            self.memorized_names.push(s);
        }
    }

    // Parses a name in the form of A@B@C@@ which represents C::B::A.
    fn read_name(&mut self) -> Result<NameSequence<'a>> {
        println!("read_name on {}", str::from_utf8(self.input)?);
        let mut names = Vec::new();
        while !self.consume(b"@") {
            println!("read_name iteration on {}", str::from_utf8(self.input)?);
            let orig = self.input;
            let name = if let Some(i) = self.consume_digit() {
                let i = i as usize;
                if i >= self.memorized_names.len() {
                    return Err(Error::new(format!(
                        "name reference too large: {}",
                        str::from_utf8(orig)?
                    )));
                }
                Name {
                    name_str: self.memorized_names[i],
                    op: None,
                    template_params: None,
                }
            } else if self.consume(b"?$") {
                // Class template.
                let name = self.read_string()?;
                println!("read_string read name {}", str::from_utf8(name)?);
                let params = self.read_params()?;
                self.expect(b"@")?; // TODO: Can this be ignored?
                Name {
                    name_str: name,
                    op: None,
                    template_params: params,
                }
            } else if self.consume(b"?") {
                // Overloaded operator.
                let (op, name) = self.read_operator()?;
                let template_params = self.read_params()?;
                Name {
                    name_str: name.unwrap_or(b""),
                    op: Some(op),
                    template_params,
                }
            } else {
                // Non-template functions or classes.
                let name = self.read_string()?;
                self.memorize_string(name);
                Name {
                    name_str: name,
                    op: None,
                    template_params: None,
                }
            };
            names.push(name);
        }

        Ok(NameSequence { names })
    }

    fn read_func_ptr(&mut self, sc: StorageClass) -> Result<Type<'a>> {
        let return_type = self.read_var_type(StorageClass::empty())?;
        let params = self.read_params()?;

        if self.input.starts_with(b"@Z") {
            self.trim(2);
        } else if self.input.starts_with(b"Z") {
            self.trim(1);
        }

        Ok(Type::Ptr(
            Box::new(Type::NonMemberFunction(
                params.unwrap_or(Params::empty()),
                StorageClass::empty(),
                Box::new(return_type),
            )),
            sc,
        ))
    }

    fn read_operator(&mut self) -> Result<(&'static str, Option<&'a [u8]>)> {
        let op = self.read_operator_name()?;
        if self.peek() != Some(b'@') {
            let op_str = self.read_string()?;
            self.memorize_string(op_str);
            Ok((op, Some(op_str)))
        } else {
            Ok((op, None))
        }
    }

    fn read_operator_name(&mut self) -> Result<&'static str> {
        let orig = self.input;

        Ok(match self.get()? {
            b'0' => "ctor",
            b'1' => "dtor",
            b'2' => " new",
            b'3' => " delete",
            b'4' => "=",
            b'5' => ">>",
            b'6' => "<<",
            b'7' => "!",
            b'8' => "==",
            b'9' => "!=",
            b'A' => "[]",
            b'C' => "->",
            b'D' => "*",
            b'E' => "++",
            b'F' => "--",
            b'G' => "-",
            b'H' => "+",
            b'I' => "&",
            b'J' => "->*",
            b'K' => "/",
            b'L' => "%",
            b'M' => "<",
            b'N' => "<=",
            b'O' => ">",
            b'P' => ">=",
            b'Q' => ",",
            b'R' => "()",
            b'S' => "~",
            b'T' => "^",
            b'U' => "|",
            b'V' => "&&",
            b'W' => "||",
            b'X' => "*=",
            b'Y' => "+=",
            b'Z' => "-=",
            b'_' => match self.get()? {
                b'0' => "/=",
                b'1' => "%=",
                b'2' => ">>=",
                b'3' => "<<=",
                b'4' => "&=",
                b'5' => "|=",
                b'6' => "^=",
                b'U' => " new[]",
                b'V' => " delete[]",
                b'_' => if self.consume(b"L") {
                    " co_await"
                } else {
                    return Err(Error::new(format!(
                        "unknown operator name: {}",
                        str::from_utf8(orig)?
                    )));
                },
                _ => {
                    return Err(Error::new(format!(
                        "unknown operator name: {}",
                        str::from_utf8(orig)?
                    )))
                }
            },
            _ => {
                return Err(Error::new(format!(
                    "unknown operator name: {}",
                    str::from_utf8(orig)?
                )))
            }
        })
    }

    fn read_func_class(&mut self) -> Result<FuncClass> {
        let orig = self.input;
        Ok(match self.get()? {
            b'A' => FuncClass::PRIVATE,
            b'B' => FuncClass::PRIVATE | FuncClass::FAR,
            b'C' => FuncClass::PRIVATE | FuncClass::STATIC,
            b'D' => FuncClass::PRIVATE | FuncClass::STATIC,
            b'E' => FuncClass::PRIVATE | FuncClass::VIRTUAL,
            b'F' => FuncClass::PRIVATE | FuncClass::VIRTUAL,
            b'I' => FuncClass::PROTECTED,
            b'J' => FuncClass::PROTECTED | FuncClass::FAR,
            b'K' => FuncClass::PROTECTED | FuncClass::STATIC,
            b'L' => FuncClass::PROTECTED | FuncClass::STATIC | FuncClass::FAR,
            b'M' => FuncClass::PROTECTED | FuncClass::VIRTUAL,
            b'N' => FuncClass::PROTECTED | FuncClass::VIRTUAL | FuncClass::FAR,
            b'Q' => FuncClass::PUBLIC,
            b'R' => FuncClass::PUBLIC | FuncClass::FAR,
            b'S' => FuncClass::PUBLIC | FuncClass::STATIC,
            b'T' => FuncClass::PUBLIC | FuncClass::STATIC | FuncClass::FAR,
            b'U' => FuncClass::PUBLIC | FuncClass::VIRTUAL,
            b'V' => FuncClass::PUBLIC | FuncClass::VIRTUAL | FuncClass::FAR,
            b'Y' => FuncClass::GLOBAL,
            b'Z' => FuncClass::GLOBAL | FuncClass::FAR,
            _ => {
                return Err(Error::new(format!(
                    "unknown func class: {}",
                    str::from_utf8(orig)?
                )))
            }
        })
    }

    fn read_func_access_class(&mut self) -> StorageClass {
        let access_class = match self.peek() {
            Some(b'A') => StorageClass::empty(),
            Some(b'B') => StorageClass::CONST,
            Some(b'C') => StorageClass::VOLATILE,
            Some(b'D') => StorageClass::CONST | StorageClass::VOLATILE,
            _ => return StorageClass::empty(),
        };
        self.trim(1);
        access_class
    }

    fn read_calling_conv(&mut self) -> Result<CallingConv> {
        let orig = self.input;

        Ok(match self.get()? {
            b'A' => CallingConv::Cdecl,
            b'B' => CallingConv::Cdecl,
            b'C' => CallingConv::Pascal,
            b'E' => CallingConv::Thiscall,
            b'G' => CallingConv::Stdcall,
            b'I' => CallingConv::Fastcall,
            _ => {
                return Err(Error::new(format!(
                    "unknown calling conv: {}",
                    str::from_utf8(orig)?
                )))
            }
        })
    }

    // <return-type> ::= <type>
    //               ::= @ # structors (they have no declared return type)
    fn read_func_return_type(&mut self, storage_class: StorageClass) -> Result<Type<'a>> {
        if self.consume(b"@") {
            Ok(Type::None)
        } else {
            self.read_var_type(storage_class)
        }
    }

    fn read_storage_class(&mut self) -> StorageClass {
        let storage_class = match self.peek() {
            Some(b'A') => StorageClass::empty(),
            Some(b'B') => StorageClass::CONST,
            Some(b'C') => StorageClass::VOLATILE,
            Some(b'D') => StorageClass::CONST | StorageClass::VOLATILE,
            Some(b'E') => StorageClass::FAR,
            Some(b'F') => StorageClass::CONST | StorageClass::FAR,
            Some(b'G') => StorageClass::VOLATILE | StorageClass::FAR,
            Some(b'H') => StorageClass::CONST | StorageClass::VOLATILE | StorageClass::FAR,
            _ => return StorageClass::empty(),
        };
        self.trim(1);
        storage_class
    }

    fn read_storage_class_for_return(&mut self) -> Result<StorageClass> {
        if !self.consume(b"?") {
            return Ok(StorageClass::empty());
        }
        let orig = self.input;

        Ok(match self.get()? {
            b'A' => StorageClass::empty(),
            b'B' => StorageClass::CONST,
            b'C' => StorageClass::VOLATILE,
            b'D' => StorageClass::CONST | StorageClass::VOLATILE,
            _ => {
                return Err(Error::new(format!(
                    "unknown storage class: {}",
                    str::from_utf8(orig)?
                )))
            }
        })
    }

    // Reads a variable type.
    fn read_var_type(&mut self, sc: StorageClass) -> Result<Type<'a>> {
        println!("read_var_type on {}", str::from_utf8(self.input)?);
        if self.consume(b"W4") {
            let name = self.read_name()?;
            return Ok(Type::Enum(name, sc));
        }

        if self.consume(b"P6A") {
            return self.read_func_ptr(sc);
        }

        let orig = self.input;

        Ok(match self.get()? {
            b'T' => Type::Union(self.read_name()?, sc),
            b'U' => Type::Struct(self.read_name()?, sc),
            b'V' => Type::Class(self.read_name()?, sc),
            b'A' => Type::Ref(Box::new(self.read_pointee()?), sc),
            b'P' => Type::Ptr(Box::new(self.read_pointee()?), sc),
            b'Q' => Type::Ptr(Box::new(self.read_pointee()?), StorageClass::CONST),
            b'Y' => self.read_array()?,
            b'X' => Type::Void(sc),
            b'D' => Type::Char(sc),
            b'C' => Type::Schar(sc),
            b'E' => Type::Uchar(sc),
            b'F' => Type::Short(sc),
            b'G' => Type::Ushort(sc),
            b'H' => Type::Int(sc),
            b'I' => Type::Uint(sc),
            b'J' => Type::Long(sc),
            b'K' => Type::Ulong(sc),
            b'M' => Type::Float(sc),
            b'N' => Type::Double(sc),
            b'O' => Type::Ldouble(sc),
            b'_' => match self.get()? {
                b'N' => Type::Bool(sc),
                b'J' => Type::Int64(sc),
                b'K' => Type::Uint64(sc),
                b'W' => Type::Wchar(sc),
                _ => {
                    return Err(Error::new(format!(
                        "unknown primitive type: {}",
                        str::from_utf8(orig)?
                    )))
                }
            },
            _ => {
                return Err(Error::new(format!(
                    "unknown primitive type: {}",
                    str::from_utf8(orig)?
                )))
            }
        })
    }

    fn read_pointee(&mut self) -> Result<Type<'a>> {
        let _is_64bit_ptr = self.expect(b"E");
        let storage_class = self.read_storage_class();
        self.read_var_type(storage_class)
    }

    fn read_array(&mut self) -> Result<Type<'a>> {
        let dimension = self.read_number()?;
        if dimension <= 0 {
            return Err(Error::new(format!(
                "invalid array dimension: {}",
                dimension
            )));
        }
        let (array, _) = self.read_nested_array(dimension)?;
        Ok(array)
    }

    fn read_nested_array(&mut self, dimension: i32) -> Result<(Type<'a>, StorageClass)> {
        if dimension > 0 {
            let len = self.read_number()?;
            let (inner_array, storage_class) = self.read_nested_array(dimension - 1)?;
            Ok((
                Type::Array(len, Box::new(inner_array), storage_class),
                storage_class,
            ))
        } else {
            let storage_class = if self.consume(b"$$C") {
                if self.consume(b"B") {
                    StorageClass::CONST
                } else if self.consume(b"C") || self.consume(b"D") {
                    StorageClass::CONST | StorageClass::VOLATILE
                } else if !self.consume(b"A") {
                    return Err(Error::new(format!(
                        "unknown storage class: {}",
                        str::from_utf8(self.input)?
                    )));
                } else {
                    StorageClass::empty()
                }
            } else {
                StorageClass::empty()
            };

            Ok((self.read_var_type(StorageClass::empty())?, storage_class))
        }
    }

    // Reads a function or a template parameters.
    fn read_params(&mut self) -> Result<Option<Params<'a>>> {
        println!("read_params on {}", str::from_utf8(self.input)?);
        // Within the same parameter list, you can backreference the first 10 types.
        let mut backref: Vec<Type<'a>> = Vec::with_capacity(10);

        let mut params: Vec<Type<'a>> = Vec::new();

        while !self.input.starts_with(b"@") && !self.input.starts_with(b"Z") {
            if let Some(n) = self.consume_digit() {
                if n as usize >= backref.len() {
                    return Err(Error::new(format!("invalid backreference: {}", n)));
                }

                params.push(backref[n as usize].clone());
                continue;
            }

            let len = self.input.len();

            let param_type = self.read_var_type(StorageClass::empty())?;

            // Single-letter types are ignored for backreferences because
            // memorizing them doesn't save anything.
            if backref.len() <= 9 && len - self.input.len() > 1 {
                backref.push(param_type.clone());
            }
            params.push(param_type);
        }
        if params.is_empty() {
            Ok(None)
        } else {
            Ok(Some(Params { types: params }))
        }
    }
}

fn demangle<'a>(input: &'a str) -> Result<String> {
    let state = ParserState {
        input: input.as_bytes(),
        memorized_names: Vec::with_capacity(10),
    };
    let parse_result = state.parse()?;
    let mut s = Vec::new();
    serialize(&mut s, &parse_result).unwrap();
    Ok(String::from_utf8(s)?)
}

// Converts an AST to a string.
//
// Converting an AST representing a C++ type to a string is tricky due
// to the bad grammar of the C++ declaration inherited from C. You have
// to construct a string from inside to outside. For example, if a type
// X is a pointer to a function returning int, the order you create a
// string becomes something like this:
//
//   (1) X is a pointer: *X
//   (2) (1) is a function returning int: int (*X)()
//
// So you cannot construct a result just by appending strings to a result.
//
// To deal with this, we split the function into two. write_pre() writes
// the "first half" of type declaration, and write_post() writes the
// "second half". For example, write_pre() writes a return type for a
// function and write_post() writes an parameter list.
fn serialize(w: &mut Vec<u8>, parse_result: &ParseResult) -> SerializeResult<()> {
    write_pre(w, &parse_result.symbol_type)?;
    write_name(w, &parse_result.symbol)?;
    write_post(w, &parse_result.symbol_type)?;
    Ok(())
}

// Write the "first half" of a given type.
fn write_pre(w: &mut Vec<u8>, t: &Type) -> SerializeResult<()> {
    let storage_class = match t {
        &Type::None => return Ok(()),
        &Type::MemberFunction(_, _, ref inner) => {
            write_pre(w, inner)?;
            return Ok(());
        }
        &Type::NonMemberFunction(_, _, ref inner) => {
            write_pre(w, inner)?;
            return Ok(());
        }
        &Type::Ptr(ref inner, storage_class) | &Type::Ref(ref inner, storage_class) => {
            write_pre(w, inner)?;

            // "[]" and "()" (for function parameters) take precedence over "*",
            // so "int *x(int)" means "x is a function returning int *". We need
            // parentheses to supercede the default precedence. (e.g. we want to
            // emit something like "int (*x)(int)".)
            match inner.as_ref() {
                &Type::MemberFunction(_, _, _)
                | &Type::NonMemberFunction(_, _, _)
                | &Type::Array(_, _, _) => {
                    write!(w, "(")?;
                }
                _ => {}
            }

            match t {
                &Type::Ptr(_, _) => { write_space(w)?; write!(w, "*")? },
                &Type::Ref(_, _) => { write_space(w)?; write!(w, "&")? },
                _ => {}
            }

            storage_class
        }
        &Type::Array(_len, ref inner, storage_class) => {
            write_pre(w, inner)?;
            storage_class
        }
        &Type::Struct(ref names, sc) => {
            write_class(w, names, "struct")?;
            sc
        }
        &Type::Union(ref names, sc) => {
            write_class(w, names, "union")?;
            sc
        }
        &Type::Class(ref names, sc) => {
            write_class(w, names, "class")?;
            sc
        }
        &Type::Enum(ref names, sc) => {
            write_class(w, names, "enum")?;
            sc
        }
        &Type::Void(sc) => {
            write!(w, "void")?;
            sc
        }
        &Type::Bool(sc) => {
            write!(w, "bool")?;
            sc
        }
        &Type::Char(sc) => {
            write!(w, "char")?;
            sc
        }
        &Type::Schar(sc) => {
            write!(w, "signed char")?;
            sc
        }
        &Type::Uchar(sc) => {
            write!(w, "unsigned char")?;
            sc
        }
        &Type::Short(sc) => {
            write!(w, "short")?;
            sc
        }
        &Type::Ushort(sc) => {
            write!(w, "unsigned short")?;
            sc
        }
        &Type::Int(sc) => {
            write!(w, "int")?;
            sc
        }
        &Type::Uint(sc) => {
            write!(w, "unsigned int")?;
            sc
        }
        &Type::Long(sc) => {
            write!(w, "long")?;
            sc
        }
        &Type::Ulong(sc) => {
            write!(w, "unsigned long")?;
            sc
        }
        &Type::Int64(sc) => {
            write!(w, "int64_t")?;
            sc
        }
        &Type::Uint64(sc) => {
            write!(w, "uint64_t")?;
            sc
        }
        &Type::Wchar(sc) => {
            write!(w, "wchar_t")?;
            sc
        }
        &Type::Float(sc) => {
            write!(w, "float")?;
            sc
        }
        &Type::Double(sc) => {
            write!(w, "double")?;
            sc
        }
        &Type::Ldouble(sc) => {
            write!(w, "long double")?;
            sc
        }
    };

    if storage_class.contains(StorageClass::CONST) {
        write_space(w)?;
        write!(w, "const")?;
    }

    Ok(())
}

// Write the "second half" of a given type.
fn write_post(w: &mut Vec<u8>, t: &Type) -> SerializeResult<()> {
    match t {
        &Type::MemberFunction(ref params, sc, _) | &Type::NonMemberFunction(ref params, sc, _) => {
            write!(w, "(")?;
            write_params(w, params)?;
            write!(w, ")")?;
            if sc.contains(StorageClass::CONST) {
                write!(w, "const")?;
            }
        }
        &Type::Ptr(ref inner, _sc) | &Type::Ref(ref inner, _sc) => {
            match inner.as_ref() {
                &Type::MemberFunction(_, _, _)
                | &Type::NonMemberFunction(_, _, _)
                | &Type::Array(_, _, _) => {
                    write!(w, ")")?;
                }
                _ => {}
            }
            write_post(w, inner)?;
        }
        &Type::Array(len, ref inner, _sc) => {
            write!(w, "[{}]", len)?;
            write_post(w, inner)?;
        }
        _ => {}
    }
    Ok(())
}

// Write a function or template parameter list.
fn write_params(w: &mut Vec<u8>, p: &Params) -> SerializeResult<()> {
    for param in p.types.iter().take(p.types.len() - 1) {
        write_pre(w, param)?;
        write_post(w, param)?;
        write!(w, ",")?;
    }
    if let Some(param) = p.types.last() {
        write_pre(w, param)?;
        write_post(w, param)?;
    }
    Ok(())
}

fn write_class(w: &mut Vec<u8>, names: &NameSequence, s: &str) -> SerializeResult<()> {
    write!(w, "{}", s)?;
    write!(w, " ")?;
    write_name(w, names)?;
    Ok(())
}

fn write_space(w: &mut Vec<u8>) -> SerializeResult<()> {
    if let Some(&c) = w.last() {
        if char::from(c).is_ascii_alphabetic() || c == b'*' || c == b'&' {
            write!(w, " ")?;
        }
    }
    Ok(())
}

// Write a name read by read_name().
fn write_name(w: &mut Vec<u8>, names: &NameSequence) -> SerializeResult<()> {
    write_space(w)?;

    // Print out namespaces or outer class names.
    for name in names.names.iter().rev().take(names.names.len() - 1) {
        w.write(name.name_str)?;
        write_tmpl_params(w, &name.template_params)?;
        write!(w, "::")?;
    }

    if let Some(name) = names.names.first() {
        match name.op {
            None => {
                // Print out a regular name.
                w.write(name.name_str)?;
                write_tmpl_params(w, &name.template_params)?;
            }
            Some(op) => {
                if op == "ctor" || op == "dtor" {
                    // Print out ctor or dtor.
                    w.write(name.name_str)?;
                    if let &Some(ref params) = &name.template_params {
                        write_params(w, params)?;
                    }
                    write!(w, "::")?;
                    if op == "dtor" {
                        write!(w, "~")?;
                    }
                    w.write(name.name_str)?;
                } else {
                    // Print out an overloaded operator.
                    if !name.name_str.is_empty() {
                        write!(w, "{}::", str::from_utf8(name.name_str)?)?;
                    }
                    write!(w, "operator{}", op)?;
                }
            }
        }
    }
    Ok(())
}

fn write_tmpl_params<'a>(w: &mut Vec<u8>, params: &Option<Params<'a>>) -> SerializeResult<()> {
    if let &Some(ref params) = params {
        write!(w, "<")?;
        write_params(w, params)?;
        write!(w, ">")?;
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        println!("{} <symbol>", args[0]);
        std::process::exit(1);
    }

    match demangle(&args[1]) {
        Ok(s) => {
            println!("{}", s);
        }
        Err(err) => {
            eprintln!("error: {:?}", err);
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    fn expect(input: &str, reference: &str) {
        let demangled: ::Result<_> = ::demangle(input);
        let reference: ::Result<_> = Ok(reference.to_owned());
        assert_eq!(demangled, reference);
    }

    // std::basic_filebuf<char,struct std::char_traits<char> >::basic_filebuf<char,struct std::char_traits<char> >
    // std::basic_filebuf<char,struct std::char_traits<char> >::"operator ctor"
    // "operator ctor" = ?0

    #[test]
    fn wine_tests() {
        // expect("??0Klass@std@@AEAA@AEBV01@@Z",
        //        "std::Klass::Klass(class std::Klass const &)");
        // expect("??0?$Klass@V?$Mass@_N@@@std@@QEAA@AEBV01@@Z",
        //        "std::Klass<class Mass<bool> >::Klass<class Mass<bool> >(class std::Klass<class Mass<bool> > const &)");
        expect("??0?$Klass@_N@std@@QEAA@AEBV01@@Z",
               "std::Klass<bool>::Klass<bool>(class std::Klass<bool> const &)");
        // expect("??0?$Klass@V?$Mass@_N@btd@@@std@@QEAA@AEBV01@@Z",
        //        "std::Klass::Klass(class std::Klass const &)");
        // expect("??0?$Klass@V?$Mass@_N@std@@@std@@QEAA@AEBV01@@Z",
        //        "std::Klass::Klass(class std::Klass const &)");
        expect("??0bad_alloc@std@@QAE@ABV01@@Z",
               "std::bad_alloc::bad_alloc(class std::bad_alloc const &)");
        expect("??0bad_alloc@std@@QAE@PBD@Z",
               "std::bad_alloc::bad_alloc(char const *)");
        expect("??0bad_cast@@AAE@PBQBD@Z",
               "bad_cast::bad_cast(char const * const *)");
        expect("??0bad_cast@@QAE@ABQBD@Z",
               "bad_cast::bad_cast(char const * const &)");
        expect("??0bad_cast@@QAE@ABV0@@Z",
               "bad_cast::bad_cast(class bad_cast const &)");
        expect("??0bad_exception@std@@QAE@ABV01@@Z",
               "std::bad_exception::bad_exception(class std::bad_exception const &)");
        expect("??0bad_exception@std@@QAE@PBD@Z",
               "std::bad_exception::bad_exception(char const *)");
        expect("??0bad_exception@std@@QAE@PBD@Z",
              "std::bad_exception::bad_exception(char const *)");
        expect("??0?$basic_filebuf@DU?$char_traits@D@std@@@std@@QAE@ABV01@@Z",
            "std::basic_filebuf<char,struct std::char_traits<char> >::basic_filebuf<char,struct std::char_traits<char> >(class std::basic_filebuf<char,struct std::char_traits<char> > const &)");
        expect("??0?$basic_filebuf@DU?$char_traits@D@std@@@std@@QAE@ABV01@@Z",
            "std::basic_filebuf<char,struct std::char_traits<char> >::basic_filebuf<char,struct std::char_traits<char> >(class std::basic_filebuf<char,struct std::char_traits<char> > const &)");
        expect("??0?$basic_filebuf@DU?$char_traits@D@std@@@std@@QAE@PAU_iobuf@@@Z",
              "std::basic_filebuf<char,struct std::char_traits<char> >::basic_filebuf<char,struct std::char_traits<char> >(struct _iobuf *)");
        expect("??0?$basic_filebuf@DU?$char_traits@D@std@@@std@@QAE@W4_Uninitialized@1@@Z",
            "std::basic_filebuf<char,struct std::char_traits<char> >::basic_filebuf<char,struct std::char_traits<char> >(enum std::_Uninitialized)");
        expect("??0?$basic_filebuf@GU?$char_traits@G@std@@@std@@QAE@ABV01@@Z",
            "std::basic_filebuf<unsigned short,struct std::char_traits<unsigned short> >::basic_filebuf<unsigned short,struct std::char_traits<unsigned short> >(class std::basic_filebuf<unsigned short,struct std::char_traits<unsigned short> > const &)");
        expect("??0?$basic_filebuf@GU?$char_traits@G@std@@@std@@QAE@PAU_iobuf@@@Z",
              "std::basic_filebuf<unsigned short,struct std::char_traits<unsigned short> >::basic_filebuf<unsigned short,struct std::char_traits<unsigned short> >(struct _iobuf *)");
        expect("??0?$basic_filebuf@GU?$char_traits@G@std@@@std@@QAE@W4_Uninitialized@1@@Z",
            "std::basic_filebuf<unsigned short,struct std::char_traits<unsigned short> >::basic_filebuf<unsigned short,struct std::char_traits<unsigned short> >(enum std::_Uninitialized)");
        expect("??0?$basic_stringstream@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QAE@ABV01@@Z",
            "std::basic_stringstream<char,struct std::char_traits<char>,class std::allocator<char> >::basic_stringstream<char,struct std::char_traits<char>,class std::allocator<char> >(class std::basic_stringstream<char,struct std::char_traits<char>,class std::allocator<char> > const &)");
        expect("??0?$basic_stringstream@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QAE@ABV?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@1@H@Z",
            "std::basic_stringstream<char,struct std::char_traits<char>,class std::allocator<char> >::basic_stringstream<char,struct std::char_traits<char>,class std::allocator<char> >(class std::basic_string<char,struct std::char_traits<char>,class std::allocator<char> > const &,int)");
        expect("??0?$basic_stringstream@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QAE@H@Z",
              "std::basic_stringstream<char,struct std::char_traits<char>,class std::allocator<char> >::basic_stringstream<char,struct std::char_traits<char>,class std::allocator<char> >(int)");
        expect("??0?$basic_stringstream@GU?$char_traits@G@std@@V?$allocator@G@2@@std@@QAE@ABV01@@Z",
            "std::basic_stringstream<unsigned short,struct std::char_traits<unsigned short>,class std::allocator<unsigned short> >::basic_stringstream<unsigned short,struct std::char_traits<unsigned short>,class std::allocator<unsigned short> >(class std::basic_stringstream<unsigned short,struct std::char_traits<unsigned short>,class std::allocator<unsigned short> > const &)");
        expect("??0?$basic_stringstream@GU?$char_traits@G@std@@V?$allocator@G@2@@std@@QAE@ABV?$basic_string@GU?$char_traits@G@std@@V?$allocator@G@2@@1@H@Z",
            "std::basic_stringstream<unsigned short,struct std::char_traits<unsigned short>,class std::allocator<unsigned short> >::basic_stringstream<unsigned short,struct std::char_traits<unsigned short>,class std::allocator<unsigned short> >(class std::basic_string<unsigned short,struct std::char_traits<unsigned short>,class std::allocator<unsigned short> > const &,int)");
        expect("??0?$basic_stringstream@GU?$char_traits@G@std@@V?$allocator@G@2@@std@@QAE@H@Z",
              "std::basic_stringstream<unsigned short,struct std::char_traits<unsigned short>,class std::allocator<unsigned short> >::basic_stringstream<unsigned short,struct std::char_traits<unsigned short>,class std::allocator<unsigned short> >(int)");
        expect("??0?$num_get@DV?$istreambuf_iterator@DU?$char_traits@D@std@@@std@@@std@@QAE@ABV_Locinfo@1@I@Z",
            "std::num_get<char,class std::istreambuf_iterator<char,struct std::char_traits<char> > >::num_get<char,class std::istreambuf_iterator<char,struct std::char_traits<char> > >(class std::_Locinfo const &,unsigned int)");
        expect("??0?$num_get@DV?$istreambuf_iterator@DU?$char_traits@D@std@@@std@@@std@@QAE@I@Z",
              "std::num_get<char,class std::istreambuf_iterator<char,struct std::char_traits<char> > >::num_get<char,class std::istreambuf_iterator<char,struct std::char_traits<char> > >(unsigned int)");
        expect("??0?$num_get@GV?$istreambuf_iterator@GU?$char_traits@G@std@@@std@@@std@@QAE@ABV_Locinfo@1@I@Z",
            "std::num_get<unsigned short,class std::istreambuf_iterator<unsigned short,struct std::char_traits<unsigned short> > >::num_get<unsigned short,class std::istreambuf_iterator<unsigned short,struct std::char_traits<unsigned short> > >(class std::_Locinfo const &,unsigned int)");
        expect("??0?$num_get@GV?$istreambuf_iterator@GU?$char_traits@G@std@@@std@@@std@@QAE@I@Z",
              "std::num_get<unsigned short,class std::istreambuf_iterator<unsigned short,struct std::char_traits<unsigned short> > >::num_get<unsigned short,class std::istreambuf_iterator<unsigned short,struct std::char_traits<unsigned short> > >(unsigned int)");
        expect("??0streambuf@@QAE@ABV0@@Z",
              "streambuf::streambuf(class streambuf const &)");
        expect("??0strstreambuf@@QAE@ABV0@@Z",
              "strstreambuf::strstreambuf(class strstreambuf const &)");
        expect("??0strstreambuf@@QAE@H@Z",
              "strstreambuf::strstreambuf(int)");
        expect("??0strstreambuf@@QAE@P6APAXJ@ZP6AXPAX@Z@Z",
              "strstreambuf::strstreambuf(void * (__cdecl*)(long),void (__cdecl*)(void *))");
        expect("??0strstreambuf@@QAE@PADH0@Z",
              "strstreambuf::strstreambuf(char *,int,char *)");
        expect("??0strstreambuf@@QAE@PAEH0@Z",
              "strstreambuf::strstreambuf(unsigned char *,int,unsigned char *)");
        expect("??0strstreambuf@@QAE@XZ",
              "strstreambuf::strstreambuf(void)");
        expect("??1__non_rtti_object@std@@UAE@XZ",
              "public: virtual __thiscall std::__non_rtti_object::~__non_rtti_object(void)");
        expect("??1__non_rtti_object@@UAE@XZ",
              "public: virtual __thiscall __non_rtti_object::~__non_rtti_object(void)");
        expect("??1?$num_get@DV?$istreambuf_iterator@DU?$char_traits@D@std@@@std@@@std@@UAE@XZ",
              "public: virtual __thiscall std::num_get<char,class std::istreambuf_iterator<char,struct std::char_traits<char> > >::~num_get<char,class std::istreambuf_iterator<char,struct std::char_traits<char> > >(void)");
        expect("??1?$num_get@GV?$istreambuf_iterator@GU?$char_traits@G@std@@@std@@@std@@UAE@XZ",
              "public: virtual __thiscall std::num_get<unsigned short,class std::istreambuf_iterator<unsigned short,struct std::char_traits<unsigned short> > >::~num_get<unsigned short,class std::istreambuf_iterator<unsigned short,struct std::char_traits<unsigned short> > >(void)");
        expect("??4istream_withassign@@QAEAAV0@ABV0@@Z",
              "public: class istream_withassign & __thiscall istream_withassign::operator=(class istream_withassign const &)");
        expect("??4istream_withassign@@QAEAAVistream@@ABV1@@Z",
              "public: class istream & __thiscall istream_withassign::operator=(class istream const &)");
        expect("??4istream_withassign@@QAEAAVistream@@PAVstreambuf@@@Z",
              "public: class istream & __thiscall istream_withassign::operator=(class streambuf *)");
        expect("??5std@@YAAAV?$basic_istream@DU?$char_traits@D@std@@@0@AAV10@AAC@Z",
              "class std::basic_istream<char,struct std::char_traits<char> > & __cdecl std::operator>>(class std::basic_istream<char,struct std::char_traits<char> > &,signed char &)");
        expect("??5std@@YAAAV?$basic_istream@DU?$char_traits@D@std@@@0@AAV10@AAD@Z",
              "class std::basic_istream<char,struct std::char_traits<char> > & __cdecl std::operator>>(class std::basic_istream<char,struct std::char_traits<char> > &,char &)");
        expect("??5std@@YAAAV?$basic_istream@DU?$char_traits@D@std@@@0@AAV10@AAE@Z",
              "class std::basic_istream<char,struct std::char_traits<char> > & __cdecl std::operator>>(class std::basic_istream<char,struct std::char_traits<char> > &,unsigned char &)");
        expect("??6?$basic_ostream@GU?$char_traits@G@std@@@std@@QAEAAV01@P6AAAVios_base@1@AAV21@@Z@Z",
              "public: class std::basic_ostream<unsigned short,struct std::char_traits<unsigned short> > & __thiscall std::basic_ostream<unsigned short,struct std::char_traits<unsigned short> >::operator<<(class std::ios_base & (__cdecl*)(class std::ios_base &))");
        expect("??6?$basic_ostream@GU?$char_traits@G@std@@@std@@QAEAAV01@PAV?$basic_streambuf@GU?$char_traits@G@std@@@1@@Z",
              "public: class std::basic_ostream<unsigned short,struct std::char_traits<unsigned short> > & __thiscall std::basic_ostream<unsigned short,struct std::char_traits<unsigned short> >::operator<<(class std::basic_streambuf<unsigned short,struct std::char_traits<unsigned short> > *)");
        expect("??6?$basic_ostream@GU?$char_traits@G@std@@@std@@QAEAAV01@PBX@Z",
              "public: class std::basic_ostream<unsigned short,struct std::char_traits<unsigned short> > & __thiscall std::basic_ostream<unsigned short,struct std::char_traits<unsigned short> >::operator<<(void const *)");
        expect("??_8?$basic_fstream@DU?$char_traits@D@std@@@std@@7B?$basic_ostream@DU?$char_traits@D@std@@@1@@",
              "const std::basic_fstream<char,struct std::char_traits<char> >::`vbtable'{for `std::basic_ostream<char,struct std::char_traits<char> >'}");
        expect("??_8?$basic_fstream@GU?$char_traits@G@std@@@std@@7B?$basic_istream@GU?$char_traits@G@std@@@1@@",
              "const std::basic_fstream<unsigned short,struct std::char_traits<unsigned short> >::`vbtable'{for `std::basic_istream<unsigned short,struct std::char_traits<unsigned short> >'}");
        expect("??_8?$basic_fstream@GU?$char_traits@G@std@@@std@@7B?$basic_ostream@GU?$char_traits@G@std@@@1@@",
              "const std::basic_fstream<unsigned short,struct std::char_traits<unsigned short> >::`vbtable'{for `std::basic_ostream<unsigned short,struct std::char_traits<unsigned short> >'}");
        expect("??9std@@YA_NPBDABV?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@0@@Z",
              "bool __cdecl std::operator!=(char const *,class std::basic_string<char,struct std::char_traits<char>,class std::allocator<char> > const &)");
        expect("??9std@@YA_NPBGABV?$basic_string@GU?$char_traits@G@std@@V?$allocator@G@2@@0@@Z",
              "bool __cdecl std::operator!=(unsigned short const *,class std::basic_string<unsigned short,struct std::char_traits<unsigned short>,class std::allocator<unsigned short> > const &)");
        expect("??A?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QAEAADI@Z",
              "public: char & __thiscall std::basic_string<char,struct std::char_traits<char>,class std::allocator<char> >::operator[](unsigned int)");
        expect("??A?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QBEABDI@Z",
              "public: char const & __thiscall std::basic_string<char,struct std::char_traits<char>,class std::allocator<char> >::operator[](unsigned int)const ");
        expect("??A?$basic_string@GU?$char_traits@G@std@@V?$allocator@G@2@@std@@QAEAAGI@Z",
              "public: unsigned short & __thiscall std::basic_string<unsigned short,struct std::char_traits<unsigned short>,class std::allocator<unsigned short> >::operator[](unsigned int)");
        expect("??A?$basic_string@GU?$char_traits@G@std@@V?$allocator@G@2@@std@@QBEABGI@Z",
              "public: unsigned short const & __thiscall std::basic_string<unsigned short,struct std::char_traits<unsigned short>,class std::allocator<unsigned short> >::operator[](unsigned int)const ");
        expect("?abs@std@@YAMABV?$complex@M@1@@Z",
              "float __cdecl std::abs(class std::complex<float> const &)");
        expect("?abs@std@@YANABV?$complex@N@1@@Z",
              "double __cdecl std::abs(class std::complex<double> const &)");
        expect("?abs@std@@YAOABV?$complex@O@1@@Z",
              "long double __cdecl std::abs(class std::complex<long double> const &)");
        expect("?cin@std@@3V?$basic_istream@DU?$char_traits@D@std@@@1@A",
              "class std::basic_istream<char,struct std::char_traits<char> > std::cin");
        expect("?do_get@?$num_get@DV?$istreambuf_iterator@DU?$char_traits@D@std@@@std@@@std@@MBE?AV?$istreambuf_iterator@DU?$char_traits@D@std@@@2@V32@0AAVios_base@2@AAHAAG@Z",
              "protected: virtual class std::istreambuf_iterator<char,struct std::char_traits<char> > __thiscall std::num_get<char,class std::istreambuf_iterator<char,struct std::char_traits<char> > >::do_get(class std::istreambuf_iterator<char,struct std::char_traits<char> >,class std::istreambuf_iterator<char,struct std::char_traits<char> >,class std::ios_base &,int &,unsigned short &)const ");
        expect("?do_get@?$num_get@DV?$istreambuf_iterator@DU?$char_traits@D@std@@@std@@@std@@MBE?AV?$istreambuf_iterator@DU?$char_traits@D@std@@@2@V32@0AAVios_base@2@AAHAAI@Z",
              "protected: virtual class std::istreambuf_iterator<char,struct std::char_traits<char> > __thiscall std::num_get<char,class std::istreambuf_iterator<char,struct std::char_traits<char> > >::do_get(class std::istreambuf_iterator<char,struct std::char_traits<char> >,class std::istreambuf_iterator<char,struct std::char_traits<char> >,class std::ios_base &,int &,unsigned int &)const ");
        expect("?do_get@?$num_get@DV?$istreambuf_iterator@DU?$char_traits@D@std@@@std@@@std@@MBE?AV?$istreambuf_iterator@DU?$char_traits@D@std@@@2@V32@0AAVios_base@2@AAHAAJ@Z",
              "protected: virtual class std::istreambuf_iterator<char,struct std::char_traits<char> > __thiscall std::num_get<char,class std::istreambuf_iterator<char,struct std::char_traits<char> > >::do_get(class std::istreambuf_iterator<char,struct std::char_traits<char> >,class std::istreambuf_iterator<char,struct std::char_traits<char> >,class std::ios_base &,int &,long &)const ");
        expect("?do_get@?$num_get@DV?$istreambuf_iterator@DU?$char_traits@D@std@@@std@@@std@@MBE?AV?$istreambuf_iterator@DU?$char_traits@D@std@@@2@V32@0AAVios_base@2@AAHAAK@Z",
              "protected: virtual class std::istreambuf_iterator<char,struct std::char_traits<char> > __thiscall std::num_get<char,class std::istreambuf_iterator<char,struct std::char_traits<char> > >::do_get(class std::istreambuf_iterator<char,struct std::char_traits<char> >,class std::istreambuf_iterator<char,struct std::char_traits<char> >,class std::ios_base &,int &,unsigned long &)const ");
        expect("?do_get@?$num_get@DV?$istreambuf_iterator@DU?$char_traits@D@std@@@std@@@std@@MBE?AV?$istreambuf_iterator@DU?$char_traits@D@std@@@2@V32@0AAVios_base@2@AAHAAM@Z",
              "protected: virtual class std::istreambuf_iterator<char,struct std::char_traits<char> > __thiscall std::num_get<char,class std::istreambuf_iterator<char,struct std::char_traits<char> > >::do_get(class std::istreambuf_iterator<char,struct std::char_traits<char> >,class std::istreambuf_iterator<char,struct std::char_traits<char> >,class std::ios_base &,int &,float &)const ");
        expect("?_query_new_handler@@YAP6AHI@ZXZ",
              "int (__cdecl*__cdecl _query_new_handler(void))(unsigned int)");
        expect("?register_callback@ios_base@std@@QAEXP6AXW4event@12@AAV12@H@ZH@Z",
              "public: void __thiscall std::ios_base::register_callback(void (__cdecl*)(enum std::ios_base::event,class std::ios_base &,int),int)");
        expect("?seekg@?$basic_istream@DU?$char_traits@D@std@@@std@@QAEAAV12@JW4seekdir@ios_base@2@@Z",
              "public: class std::basic_istream<char,struct std::char_traits<char> > & __thiscall std::basic_istream<char,struct std::char_traits<char> >::seekg(long,enum std::ios_base::seekdir)");
        expect("?seekg@?$basic_istream@DU?$char_traits@D@std@@@std@@QAEAAV12@V?$fpos@H@2@@Z",
              "public: class std::basic_istream<char,struct std::char_traits<char> > & __thiscall std::basic_istream<char,struct std::char_traits<char> >::seekg(class std::fpos<int>)");
        expect("?seekg@?$basic_istream@GU?$char_traits@G@std@@@std@@QAEAAV12@JW4seekdir@ios_base@2@@Z",
              "public: class std::basic_istream<unsigned short,struct std::char_traits<unsigned short> > & __thiscall std::basic_istream<unsigned short,struct std::char_traits<unsigned short> >::seekg(long,enum std::ios_base::seekdir)");
        expect("?seekg@?$basic_istream@GU?$char_traits@G@std@@@std@@QAEAAV12@V?$fpos@H@2@@Z",
              "public: class std::basic_istream<unsigned short,struct std::char_traits<unsigned short> > & __thiscall std::basic_istream<unsigned short,struct std::char_traits<unsigned short> >::seekg(class std::fpos<int>)");
        expect("?seekoff@?$basic_filebuf@DU?$char_traits@D@std@@@std@@MAE?AV?$fpos@H@2@JW4seekdir@ios_base@2@H@Z",
              "protected: virtual class std::fpos<int> __thiscall std::basic_filebuf<char,struct std::char_traits<char> >::seekoff(long,enum std::ios_base::seekdir,int)");
        expect("?seekoff@?$basic_filebuf@GU?$char_traits@G@std@@@std@@MAE?AV?$fpos@H@2@JW4seekdir@ios_base@2@H@Z",
              "protected: virtual class std::fpos<int> __thiscall std::basic_filebuf<unsigned short,struct std::char_traits<unsigned short> >::seekoff(long,enum std::ios_base::seekdir,int)");
        expect("?set_new_handler@@YAP6AXXZP6AXXZ@Z",
              "void (__cdecl*__cdecl set_new_handler(void (__cdecl*)(void)))(void)");
        expect("?str@?$basic_istringstream@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QAEXABV?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@2@@Z",
              "public: void __thiscall std::basic_istringstream<char,struct std::char_traits<char>,class std::allocator<char> >::str(class std::basic_string<char,struct std::char_traits<char>,class std::allocator<char> > const &)");
        expect("?str@?$basic_istringstream@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QBE?AV?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@2@XZ",
              "public: class std::basic_string<char,struct std::char_traits<char>,class std::allocator<char> > __thiscall std::basic_istringstream<char,struct std::char_traits<char>,class std::allocator<char> >::str(void)const ");
        expect("?str@?$basic_istringstream@GU?$char_traits@G@std@@V?$allocator@G@2@@std@@QAEXABV?$basic_string@GU?$char_traits@G@std@@V?$allocator@G@2@@2@@Z",
              "public: void __thiscall std::basic_istringstream<unsigned short,struct std::char_traits<unsigned short>,class std::allocator<unsigned short> >::str(class std::basic_string<unsigned short,struct std::char_traits<unsigned short>,class std::allocator<unsigned short> > const &)");
        expect("?str@?$basic_istringstream@GU?$char_traits@G@std@@V?$allocator@G@2@@std@@QBE?AV?$basic_string@GU?$char_traits@G@std@@V?$allocator@G@2@@2@XZ",
              "public: class std::basic_string<unsigned short,struct std::char_traits<unsigned short>,class std::allocator<unsigned short> > __thiscall std::basic_istringstream<unsigned short,struct std::char_traits<unsigned short>,class std::allocator<unsigned short> >::str(void)const ");
        expect("?str@?$basic_ostringstream@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QAEXABV?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@2@@Z",
              "public: void __thiscall std::basic_ostringstream<char,struct std::char_traits<char>,class std::allocator<char> >::str(class std::basic_string<char,struct std::char_traits<char>,class std::allocator<char> > const &)");
        expect("?str@?$basic_ostringstream@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QBE?AV?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@2@XZ",
              "public: class std::basic_string<char,struct std::char_traits<char>,class std::allocator<char> > __thiscall std::basic_ostringstream<char,struct std::char_traits<char>,class std::allocator<char> >::str(void)const ");
        expect("?str@?$basic_ostringstream@GU?$char_traits@G@std@@V?$allocator@G@2@@std@@QAEXABV?$basic_string@GU?$char_traits@G@std@@V?$allocator@G@2@@2@@Z",
              "public: void __thiscall std::basic_ostringstream<unsigned short,struct std::char_traits<unsigned short>,class std::allocator<unsigned short> >::str(class std::basic_string<unsigned short,struct std::char_traits<unsigned short>,class std::allocator<unsigned short> > const &)");
        expect("?str@?$basic_ostringstream@GU?$char_traits@G@std@@V?$allocator@G@2@@std@@QBE?AV?$basic_string@GU?$char_traits@G@std@@V?$allocator@G@2@@2@XZ",
              "public: class std::basic_string<unsigned short,struct std::char_traits<unsigned short>,class std::allocator<unsigned short> > __thiscall std::basic_ostringstream<unsigned short,struct std::char_traits<unsigned short>,class std::allocator<unsigned short> >::str(void)const ");
        expect("?str@?$basic_stringbuf@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QAEXABV?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@2@@Z",
              "public: void __thiscall std::basic_stringbuf<char,struct std::char_traits<char>,class std::allocator<char> >::str(class std::basic_string<char,struct std::char_traits<char>,class std::allocator<char> > const &)");
        expect("?str@?$basic_stringbuf@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QBE?AV?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@2@XZ",
              "public: class std::basic_string<char,struct std::char_traits<char>,class std::allocator<char> > __thiscall std::basic_stringbuf<char,struct std::char_traits<char>,class std::allocator<char> >::str(void)const ");
        expect("?str@?$basic_stringbuf@GU?$char_traits@G@std@@V?$allocator@G@2@@std@@QAEXABV?$basic_string@GU?$char_traits@G@std@@V?$allocator@G@2@@2@@Z",
              "public: void __thiscall std::basic_stringbuf<unsigned short,struct std::char_traits<unsigned short>,class std::allocator<unsigned short> >::str(class std::basic_string<unsigned short,struct std::char_traits<unsigned short>,class std::allocator<unsigned short> > const &)");
        expect("?str@?$basic_stringbuf@GU?$char_traits@G@std@@V?$allocator@G@2@@std@@QBE?AV?$basic_string@GU?$char_traits@G@std@@V?$allocator@G@2@@2@XZ",
              "public: class std::basic_string<unsigned short,struct std::char_traits<unsigned short>,class std::allocator<unsigned short> > __thiscall std::basic_stringbuf<unsigned short,struct std::char_traits<unsigned short>,class std::allocator<unsigned short> >::str(void)const ");
        expect("?str@?$basic_stringstream@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QAEXABV?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@2@@Z",
              "public: void __thiscall std::basic_stringstream<char,struct std::char_traits<char>,class std::allocator<char> >::str(class std::basic_string<char,struct std::char_traits<char>,class std::allocator<char> > const &)");
        expect("?str@?$basic_stringstream@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QBE?AV?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@2@XZ",
              "public: class std::basic_string<char,struct std::char_traits<char>,class std::allocator<char> > __thiscall std::basic_stringstream<char,struct std::char_traits<char>,class std::allocator<char> >::str(void)const ");
        expect("?str@?$basic_stringstream@GU?$char_traits@G@std@@V?$allocator@G@2@@std@@QAEXABV?$basic_string@GU?$char_traits@G@std@@V?$allocator@G@2@@2@@Z",
              "public: void __thiscall std::basic_stringstream<unsigned short,struct std::char_traits<unsigned short>,class std::allocator<unsigned short> >::str(class std::basic_string<unsigned short,struct std::char_traits<unsigned short>,class std::allocator<unsigned short> > const &)");
        expect("?str@?$basic_stringstream@GU?$char_traits@G@std@@V?$allocator@G@2@@std@@QBE?AV?$basic_string@GU?$char_traits@G@std@@V?$allocator@G@2@@2@XZ",
              "public: class std::basic_string<unsigned short,struct std::char_traits<unsigned short>,class std::allocator<unsigned short> > __thiscall std::basic_stringstream<unsigned short,struct std::char_traits<unsigned short>,class std::allocator<unsigned short> >::str(void)const ");
        expect("?_Sync@ios_base@std@@0_NA",
              "private: static bool std::ios_base::_Sync");
        expect("??_U@YAPAXI@Z",
              "void * __cdecl operator new[](unsigned int)");
        expect("??_V@YAXPAX@Z",
              "void __cdecl operator delete[](void *)");
        expect("??X?$_Complex_base@M@std@@QAEAAV01@ABM@Z",
              "public: class std::_Complex_base<float> & __thiscall std::_Complex_base<float>::operator*=(float const &)");
        expect("??Xstd@@YAAAV?$complex@M@0@AAV10@ABV10@@Z",
              "class std::complex<float> & __cdecl std::operator*=(class std::complex<float> &,class std::complex<float> const &)");
        expect("?aaa@@YAHAAUbbb@@@Z",
              "int __cdecl aaa(struct bbb &)");
        expect("?aaa@@YAHBAUbbb@@@Z",
              "int __cdecl aaa(struct bbb & volatile)");
        expect("?aaa@@YAHPAUbbb@@@Z",
              "int __cdecl aaa(struct bbb *)");
        expect("?aaa@@YAHQAUbbb@@@Z",
              "int __cdecl aaa(struct bbb * const)");
        expect("?aaa@@YAHRAUbbb@@@Z",
              "int __cdecl aaa(struct bbb * volatile)");
        expect("?aaa@@YAHSAUbbb@@@Z",
              "int __cdecl aaa(struct bbb * const volatile)");
        expect("??0aa.a@@QAE@XZ",
              "??0aa.a@@QAE@XZ");
        expect("??0aa$_3a@@QAE@XZ",
              "aa$_3a::aa$_3a(void)");
        expect("??2?$aaa@AAUbbb@@AAUccc@@AAU2@@ddd@1eee@2@QAEHXZ",
              "public: int __thiscall eee::eee::ddd::ddd::aaa<struct bbb &,struct ccc &,struct ccc &>::operator new(void)");
        expect("?pSW@@3P6GHKPAX0PAU_tagSTACKFRAME@@0P6GH0K0KPAK@ZP6GPAX0K@ZP6GK0K@ZP6GK00PAU_tagADDRESS@@@Z@ZA",
              "int (__stdcall* pSW)(unsigned long,void *,void *,struct _tagSTACKFRAME *,void *,int (__stdcall*)(void *,unsigned long,void *,unsigned long,unsigned long *),void * (__stdcall*)(void *,unsigned long),unsigned long (__stdcall*)(void *,unsigned long),unsigned long (__stdcall*)(void *,void *,struct _tagADDRESS *))");
        expect("?$_aaa@Vbbb@@",
              "_aaa<class bbb>");
        expect("?$aaa@Vbbb@ccc@@Vddd@2@",
              "aaa<class ccc::bbb,class ccc::ddd>");
        expect( "??0?$Foo@P6GHPAX0@Z@@QAE@PAD@Z",
              "Foo<int (__stdcall*)(void *,void *)>::Foo<int (__stdcall*)(void *,void *)>(char *)");
        expect( "??0?$Foo@P6GHPAX0@Z@@QAE@PAD@Z",
              "__thiscall Foo<int (__stdcall*)(void *,void *)>::Foo<int (__stdcall*)(void *,void *)>(char *)");
        expect( "?Qux@Bar@@0PAP6AHPAV1@AAH1PAH@ZA",
              "private: static int (__cdecl** Bar::Qux)(class Bar *,int &,int &,int *)" );
        expect( "?Qux@Bar@@0PAP6AHPAV1@AAH1PAH@ZA",
              "Bar::Qux");
        expect("?$AAA@$DBAB@",
              "AAA<`template-parameter257'>");
        expect("?$AAA@?C@",
              "AAA<`template-parameter-2'>");
        expect("?$AAA@PAUBBB@@",
              "AAA<struct BBB *>");
        expect("??$ccccc@PAVaaa@@@bar@bb@foo@@DGPAV0@PAV0@PAVee@@IPAPAVaaa@@1@Z",
            "private: static class bar * __stdcall foo::bb::bar::ccccc<class aaa *>(class bar *,class ee *,unsigned int,class aaa * *,class ee *)");
        expect("?f@T@@QAEHQCY1BE@BO@D@Z",
              "public: int __thiscall T::f(char (volatile * const)[20][30])");
        expect("?f@T@@QAEHQAY2BE@BO@CI@D@Z",
              "public: int __thiscall T::f(char (* const)[20][30][40])");
        expect("?f@T@@QAEHQAY1BE@BO@$$CBD@Z",
              "public: int __thiscall T::f(char const (* const)[20][30])");
        expect("??0?$Foo@U?$vector_c@H$00$01$0?1$0A@$0A@$0HPPPPPPP@$0HPPPPPPP@$0HPPPPPPP@$0HPPPPPPP@$0HPPPPPPP@$0HPPPPPPP@$0HPPPPPPP@$0HPPPPPPP@$0HPPPPPPP@$0HPPPPPPP@$0HPPPPPPP@$0HPPPPPPP@$0HPPPPPPP@$0HPPPPPPP@$0HPPPPPPP@@mpl@boost@@@@QAE@XZ",
              "Foo<struct boost::mpl::vector_c<int,1,2,-2,0,0,2147483647,2147483647,2147483647,2147483647,2147483647,2147483647,2147483647,2147483647,2147483647,2147483647,2147483647,2147483647,2147483647,2147483647,2147483647> >::Foo<struct boost::mpl::vector_c<int,1,2,-2,0,0,2147483647,2147483647,2147483647,2147483647,2147483647,2147483647,2147483647,2147483647,2147483647,2147483647,2147483647,2147483647,2147483647,2147483647,2147483647> >(void)");
        expect("?swprintf@@YAHPAGIPBGZZ",
              "int __cdecl swprintf(unsigned short *,unsigned int,unsigned short const *,...)");
        expect("?vswprintf@@YAHPAGIPBGPAD@Z",
              "int __cdecl vswprintf(unsigned short *,unsigned int,unsigned short const *,char *)");
        expect("?vswprintf@@YAHPA_WIPB_WPAD@Z",
              "int __cdecl vswprintf(wchar_t *,unsigned int,wchar_t const *,char *)");
        expect("?swprintf@@YAHPA_WIPB_WZZ",
              "int __cdecl swprintf(wchar_t *,unsigned int,wchar_t const *,...)");
        expect("??Xstd@@YAAEAV?$complex@M@0@AEAV10@AEBV10@@Z",
              "class std::complex<float> & __ptr64 __cdecl std::operator*=(class std::complex<float> & __ptr64,class std::complex<float> const & __ptr64)");
        expect("?_Doraise@bad_cast@std@@MEBAXXZ",
              "protected: virtual void __cdecl std::bad_cast::_Doraise(void)const __ptr64");
        expect("??$?DM@std@@YA?AV?$complex@M@0@ABMABV10@@Z",
            "class std::complex<float> __cdecl std::operator*<float>(float const &,class std::complex<float> const &)");
        expect("?_R2@?BN@???$_Fabs@N@std@@YANAEBV?$complex@N@1@PEAH@Z@4NB",
            "double const `double __cdecl std::_Fabs<double>(class std::complex<double> const & __ptr64,int * __ptr64)'::`29'::_R2");
        expect("?vtordisp_thunk@std@@$4PPPPPPPM@3EAA_NXZ",
            "[thunk]:public: virtual bool __cdecl std::vtordisp_thunk`vtordisp{4294967292,4}' (void) __ptr64");
        expect("??_9CView@@$BBII@AE",
            "[thunk]: __thiscall CView::`vcall'{392,{flat}}' }'");
        expect("?_dispatch@_impl_Engine@SalomeApp@@$R4CE@BA@PPPPPPPM@7AE_NAAVomniCallHandle@@@Z",
            "[thunk]:public: virtual bool __thiscall SalomeApp::_impl_Engine::_dispatch`vtordispex{36,16,4294967292,8}' (class omniCallHandle &)");
        expect("?_Doraise@bad_cast@std@@MEBAXXZ",
              "protected: virtual void __cdecl std::bad_cast::_Doraise(void)");
        expect("??Xstd@@YAAEAV?$complex@M@0@AEAV10@AEBV10@@Z",
              "class std::complex<float> & ptr64 cdecl std::operator*=(class std::complex<float> & ptr64,class std::complex<float> const & ptr64)");
        expect("??Xstd@@YAAEAV?$complex@M@0@AEAV10@AEBV10@@Z",
            "class std::complex<float> & std::operator*=(class std::complex<float> &,class std::complex<float> const &)");
        expect("??$run@XVTask_Render_Preview@@@QtConcurrent@@YA?AV?$QFuture@X@@PEAVTask_Render_Preview@@P82@EAAXXZ@Z",
            "class QFuture<void> __cdecl QtConcurrent::run<void,class Task_Render_Preview>(class Task_Render_Preview * __ptr64,void (__cdecl Task_Render_Preview::*)(void) __ptr64)");
        expect("??_E?$TStrArray@$$BY0BAA@D$0BA@@@UAEPAXI@Z",
              "public: virtual void * __thiscall TStrArray<char [256],16>::`vector deleting destructor'(unsigned int)");
    }


    fn upstream_tests() {
        expect("?x@@3HA",
                "int x");
        expect("?x@@3PEAHEA",
                "int*x");
        expect("?x@@3PEAPEAHEA",
                "int**x");
        expect("?x@@3PEAY02HEA",
                "int(*x)[3]");
        expect("?x@@3PEAY124HEA",
                "int(*x)[3][5]");
        expect("?x@@3PEAY02$$CBHEA",
                "int const(*x)[3]");
        expect("?x@@3PEAEEA",
                "unsigned char*x");
        expect("?x@@3PEAY1NKM@5HEA",
                "int(*x)[3500][6]");
        expect("?x@@YAXMH@Z",
                "void x(float,int)");
        expect("?x@@YAXMH@Z",
                "void x(float,int)");
        expect("?x@@3P6AHMNH@ZEA",
                "int(*x)(float,double,int)");
        expect("?x@@3P6AHP6AHM@ZN@ZEA",
                "int(*x)(int(*)(float),double)");
        expect("?x@@3P6AHP6AHM@Z0@ZEA",
                "int(*x)(int(*)(float),int(*)(float))");

        expect("?x@ns@@3HA",
                "int ns::x");

        // Microsoft's undname returns "int const * const x" for this symbol.
        // I believe it's their bug.
        expect("?x@@3PEBHEB",
                "int const*x");

        expect("?x@@3QEAHEB",
                "int*const x");
        expect("?x@@3QEBHEB",
                "int const*const x");

        expect("?x@@3AEBHEB",
                "int const&x");

        expect("?x@@3PEAUty@@EA",
                "struct ty*x");
        expect("?x@@3PEATty@@EA",
                "union ty*x");
        expect("?x@@3PEAUty@@EA",
                "struct ty*x");
        expect("?x@@3PEAW4ty@@EA",
                "enum ty*x");
        expect("?x@@3PEAVty@@EA",
                "class ty*x");

        expect("?x@@3PEAV?$tmpl@H@@EA",
                "class tmpl<int>*x");
        expect("?x@@3PEAU?$tmpl@H@@EA",
                "struct tmpl<int>*x");
        expect("?x@@3PEAT?$tmpl@H@@EA",
                "union tmpl<int>*x");
        expect("?instance@@3Vklass@@A",
                "class klass instance");
        expect("?instance$initializer$@@3P6AXXZEA",
                "void(*instance$initializer$)(void)");
        expect("??0klass@@QEAA@XZ",
                "klass::klass(void)");
        expect("??1klass@@QEAA@XZ",
                "klass::~klass(void)");
        expect("?x@@YAHPEAVklass@@AEAV1@@Z",
                "int x(class klass*,class klass&)");
        expect("?x@ns@@3PEAV?$klass@HH@1@EA",
                "class ns::klass<int,int>*ns::x");
        expect("?fn@?$klass@H@ns@@QEBAIXZ",
                "unsigned int ns::klass<int>::fn(void)const");

        expect("??4klass@@QEAAAEBV0@AEBV0@@Z",
                "class klass const&klass::operator=(class klass const&)");
        expect("??7klass@@QEAA_NXZ",
                "bool klass::operator!(void)");
        expect("??8klass@@QEAA_NAEBV0@@Z",
                "bool klass::operator==(class klass const&)");
        expect("??9klass@@QEAA_NAEBV0@@Z",
                "bool klass::operator!=(class klass const&)");
        expect("??Aklass@@QEAAH_K@Z",
                "int klass::operator[](uint64_t)");
        expect("??Cklass@@QEAAHXZ",
                "int klass::operator->(void)");
        expect("??Dklass@@QEAAHXZ",
                "int klass::operator*(void)");
        expect("??Eklass@@QEAAHXZ",
                "int klass::operator++(void)");
        expect("??Eklass@@QEAAHH@Z",
                "int klass::operator++(int)");
        expect("??Fklass@@QEAAHXZ",
                "int klass::operator--(void)");
        expect("??Fklass@@QEAAHH@Z",
                "int klass::operator--(int)");
        expect("??Hklass@@QEAAHH@Z",
                "int klass::operator+(int)");
        expect("??Gklass@@QEAAHH@Z",
                "int klass::operator-(int)");
        expect("??Iklass@@QEAAHH@Z",
                "int klass::operator&(int)");
        expect("??Jklass@@QEAAHH@Z",
                "int klass::operator->*(int)");
        expect("??Kklass@@QEAAHH@Z",
                "int klass::operator/(int)");
        expect("??Mklass@@QEAAHH@Z",
                "int klass::operator<(int)");
        expect("??Nklass@@QEAAHH@Z",
                "int klass::operator<=(int)");
        expect("??Oklass@@QEAAHH@Z",
                "int klass::operator>(int)");
        expect("??Pklass@@QEAAHH@Z",
                "int klass::operator>=(int)");
        expect("??Qklass@@QEAAHH@Z",
                "int klass::operator,(int)");
        expect("??Rklass@@QEAAHH@Z",
                "int klass::operator()(int)");
        expect("??Sklass@@QEAAHXZ",
                "int klass::operator~(void)");
        expect("??Tklass@@QEAAHH@Z",
                "int klass::operator^(int)");
        expect("??Uklass@@QEAAHH@Z",
                "int klass::operator|(int)");
        expect("??Vklass@@QEAAHH@Z",
                "int klass::operator&&(int)");
        expect("??Wklass@@QEAAHH@Z",
                "int klass::operator||(int)");
        expect("??Xklass@@QEAAHH@Z",
                "int klass::operator*=(int)");
        expect("??Yklass@@QEAAHH@Z",
                "int klass::operator+=(int)");
        expect("??Zklass@@QEAAHH@Z",
                "int klass::operator-=(int)");
        expect("??_0klass@@QEAAHH@Z",
                "int klass::operator/=(int)");
        expect("??_1klass@@QEAAHH@Z",
                "int klass::operator%=(int)");
        expect("??_2klass@@QEAAHH@Z",
                "int klass::operator>>=(int)");
        expect("??_3klass@@QEAAHH@Z",
                "int klass::operator<<=(int)");
        expect("??_6klass@@QEAAHH@Z",
                "int klass::operator^=(int)");
        expect("??6@YAAEBVklass@@AEBV0@H@Z",
                "class klass const&operator<<(class klass const&,int)");
        expect("??5@YAAEBVklass@@AEBV0@_K@Z",
                "class klass const&operator>>(class klass const&,uint64_t)");
        expect("??2@YAPEAX_KAEAVklass@@@Z",
                "void*operator new(uint64_t,class klass&)");
        expect("??_U@YAPEAX_KAEAVklass@@@Z",
                "void*operator new[](uint64_t,class klass&)");
        expect("??3@YAXPEAXAEAVklass@@@Z",
                "void operator delete(void*,class klass&)");
        expect("??_V@YAXPEAXAEAVklass@@@Z",
                "void operator delete[](void*,class klass&)");
    }
}