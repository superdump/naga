use crate::{
    proc::ensure_block_returns, Arena, BinaryOperator, Block, EntryPoint, Expression, Function,
    FunctionArgument, FunctionResult, Handle, ImageQuery, LocalVariable, MathFunction,
    RelationalFunction, SampleLevel, ScalarKind, Statement, StructMember, SwizzleComponent, Type,
    TypeInner, VectorSize,
};

use super::{ast::*, error::ErrorKind, SourceMetadata};

impl Program<'_> {
    pub fn function_call(
        &mut self,
        ctx: &mut Context,
        body: &mut Block,
        fc: FunctionCallKind,
        raw_args: &[Handle<HirExpr>],
        meta: SourceMetadata,
    ) -> Result<Option<Handle<Expression>>, ErrorKind> {
        let args: Vec<_> = raw_args
            .iter()
            .map(|e| ctx.lower_expect(self, *e, false, body))
            .collect::<Result<_, _>>()?;

        match fc {
            FunctionCallKind::TypeConstructor(ty) => {
                let h = if args.len() == 1 {
                    let is_vec = match *self.resolve_type(ctx, args[0].0, args[0].1)? {
                        TypeInner::Vector { .. } => true,
                        _ => false,
                    };

                    match self.module.types[ty].inner {
                        TypeInner::Vector { size, kind, .. } if !is_vec => {
                            let (mut value, meta) = args[0];
                            ctx.implicit_conversion(self, &mut value, meta, kind)?;

                            ctx.add_expression(Expression::Splat { size, value }, body)
                        }
                        TypeInner::Scalar { kind, width } => ctx.add_expression(
                            Expression::As {
                                kind,
                                expr: args[0].0,
                                convert: Some(width),
                            },
                            body,
                        ),
                        TypeInner::Vector { size, kind, width } => {
                            let expr = ctx.add_expression(
                                Expression::Swizzle {
                                    size,
                                    vector: args[0].0,
                                    pattern: SwizzleComponent::XYZW,
                                },
                                body,
                            );

                            ctx.add_expression(
                                Expression::As {
                                    kind,
                                    expr,
                                    convert: Some(width),
                                },
                                body,
                            )
                        }
                        TypeInner::Matrix { columns, rows, .. } => {
                            // TODO: casts
                            // `Expression::As` doesn't support matrix width
                            // casts so we need to do some extra work for casts

                            let (mut value, meta) = args[0];
                            ctx.implicit_conversion(self, &mut value, meta, ScalarKind::Float)?;
                            let column = match *self.resolve_type(ctx, args[0].0, args[0].1)? {
                                TypeInner::Scalar { .. } => ctx
                                    .add_expression(Expression::Splat { size: rows, value }, body),
                                TypeInner::Matrix { .. } => {
                                    let mut components = Vec::new();

                                    for n in 0..columns as u32 {
                                        let vector = ctx.add_expression(
                                            Expression::AccessIndex {
                                                base: value,
                                                index: n,
                                            },
                                            body,
                                        );

                                        let c = ctx.add_expression(
                                            Expression::Swizzle {
                                                size: rows,
                                                vector,
                                                pattern: SwizzleComponent::XYZW,
                                            },
                                            body,
                                        );

                                        components.push(c)
                                    }

                                    let h = ctx.add_expression(
                                        Expression::Compose { ty, components },
                                        body,
                                    );

                                    return Ok(Some(h));
                                }
                                _ => value,
                            };

                            let columns =
                                std::iter::repeat(column).take(columns as usize).collect();

                            ctx.add_expression(
                                Expression::Compose {
                                    ty,
                                    components: columns,
                                },
                                body,
                            )
                        }
                        TypeInner::Struct { .. } => ctx.add_expression(
                            Expression::Compose {
                                ty,
                                components: args.into_iter().map(|arg| arg.0).collect(),
                            },
                            body,
                        ),
                        _ => return Err(ErrorKind::SemanticError(meta, "Bad cast".into())),
                    }
                } else {
                    let mut components = Vec::with_capacity(args.len());

                    for (mut arg, meta) in args.iter().copied() {
                        if let Some(kind) = self.module.types[ty].inner.scalar_kind() {
                            ctx.implicit_conversion(self, &mut arg, meta, kind)?;
                        }
                        components.push(arg)
                    }

                    ctx.add_expression(Expression::Compose { ty, components }, body)
                };

                Ok(Some(h))
            }
            FunctionCallKind::Function(name) => {
                match name.as_str() {
                    "sampler2D" => {
                        if args.len() != 2 {
                            return Err(ErrorKind::wrong_function_args(name, 2, args.len(), meta));
                        }
                        ctx.samplers.insert(args[0].0, args[1].0);
                        Ok(Some(args[0].0))
                    }
                    "texture" => {
                        if !(2..=3).contains(&args.len()) {
                            return Err(ErrorKind::wrong_function_args(name, 2, args.len(), meta));
                        }
                        if let Some(sampler) = ctx.samplers.get(&args[0].0).copied() {
                            Ok(Some(ctx.add_expression(
                                Expression::ImageSample {
                                    image: args[0].0,
                                    sampler,
                                    coordinate: args[1].0,
                                    array_index: None, //TODO
                                    offset: None,      //TODO
                                    level: args.get(2).map_or(SampleLevel::Auto, |&(expr, _)| {
                                        SampleLevel::Bias(expr)
                                    }),
                                    depth_ref: None,
                                },
                                body,
                            )))
                        } else {
                            Err(ErrorKind::SemanticError(meta, "Bad call to texture".into()))
                        }
                    }
                    "textureLod" => {
                        if args.len() != 3 {
                            return Err(ErrorKind::wrong_function_args(name, 3, args.len(), meta));
                        }
                        let exact = ctx.add_expression(
                            Expression::As {
                                kind: crate::ScalarKind::Float,
                                expr: args[2].0,
                                convert: Some(4),
                            },
                            body,
                        );
                        if let Some(sampler) = ctx.samplers.get(&args[0].0).copied() {
                            Ok(Some(ctx.add_expression(
                                Expression::ImageSample {
                                    image: args[0].0,
                                    sampler,
                                    coordinate: args[1].0,
                                    array_index: None, //TODO
                                    offset: None,      //TODO
                                    level: SampleLevel::Exact(exact),
                                    depth_ref: None,
                                },
                                body,
                            )))
                        } else {
                            Err(ErrorKind::SemanticError(
                                meta,
                                "Bad call to textureLod".into(),
                            ))
                        }
                    }
                    "textureSize" => {
                        if !(1..=2).contains(&args.len()) {
                            return Err(ErrorKind::wrong_function_args(name, 1, args.len(), meta));
                        }

                        Ok(Some(ctx.add_expression(
                            Expression::ImageQuery {
                                image: args[0].0,
                                query: ImageQuery::Size {
                                    level: args.get(1).map(|e| e.0),
                                },
                            },
                            body,
                        )))
                    }
                    "texelFetch" => {
                        if args.len() != 3 {
                            return Err(ErrorKind::wrong_function_args(name, 3, args.len(), meta));
                        }
                        if ctx.samplers.get(&args[0].0).is_some() {
                            let (arrayed, dims) =
                                match *self.resolve_type(ctx, args[0].0, args[0].1)? {
                                    TypeInner::Image { arrayed, dim, .. } => (arrayed, dim),
                                    _ => (false, crate::ImageDimension::D1),
                                };

                            let (coordinate, array_index) = if arrayed {
                                (
                                    match dims {
                                        crate::ImageDimension::D1 => ctx.add_expression(
                                            Expression::AccessIndex {
                                                base: args[1].0,
                                                index: 0,
                                            },
                                            body,
                                        ),
                                        crate::ImageDimension::D2 => ctx.add_expression(
                                            Expression::Swizzle {
                                                size: VectorSize::Bi,
                                                vector: args[1].0,
                                                pattern: SwizzleComponent::XYZW,
                                            },
                                            body,
                                        ),
                                        _ => ctx.add_expression(
                                            Expression::Swizzle {
                                                size: VectorSize::Tri,
                                                vector: args[1].0,
                                                pattern: SwizzleComponent::XYZW,
                                            },
                                            body,
                                        ),
                                    },
                                    Some(ctx.add_expression(
                                        Expression::AccessIndex {
                                            base: args[1].0,
                                            index: match dims {
                                                crate::ImageDimension::D1 => 1,
                                                crate::ImageDimension::D2 => 2,
                                                crate::ImageDimension::D3 => 3,
                                                crate::ImageDimension::Cube => 2,
                                            },
                                        },
                                        body,
                                    )),
                                )
                            } else {
                                (args[1].0, None)
                            };

                            Ok(Some(ctx.add_expression(
                                Expression::ImageLoad {
                                    image: args[0].0,
                                    coordinate,
                                    array_index,
                                    index: Some(args[2].0),
                                },
                                body,
                            )))
                        } else {
                            Err(ErrorKind::SemanticError(
                                meta,
                                "Bad call to texelFetch".into(),
                            ))
                        }
                    }
                    "ceil" | "round" | "floor" | "fract" | "trunc" | "sin" | "abs" | "sqrt"
                    | "inversesqrt" | "exp" | "exp2" | "sign" | "transpose" | "inverse"
                    | "normalize" | "sinh" | "cos" | "cosh" | "tan" | "tanh" | "acos" | "asin"
                    | "log" | "log2" | "length" | "determinant" | "bitCount"
                    | "bitfieldReverse" => {
                        if args.len() != 1 {
                            return Err(ErrorKind::wrong_function_args(name, 1, args.len(), meta));
                        }
                        Ok(Some(ctx.add_expression(
                            Expression::Math {
                                fun: match name.as_str() {
                                    "ceil" => MathFunction::Ceil,
                                    "round" => MathFunction::Round,
                                    "floor" => MathFunction::Floor,
                                    "fract" => MathFunction::Fract,
                                    "trunc" => MathFunction::Trunc,
                                    "sin" => MathFunction::Sin,
                                    "abs" => MathFunction::Abs,
                                    "sqrt" => MathFunction::Sqrt,
                                    "inversesqrt" => MathFunction::InverseSqrt,
                                    "exp" => MathFunction::Exp,
                                    "exp2" => MathFunction::Exp2,
                                    "sign" => MathFunction::Sign,
                                    "transpose" => MathFunction::Transpose,
                                    "inverse" => MathFunction::Inverse,
                                    "normalize" => MathFunction::Normalize,
                                    "sinh" => MathFunction::Sinh,
                                    "cos" => MathFunction::Cos,
                                    "cosh" => MathFunction::Cosh,
                                    "tan" => MathFunction::Tan,
                                    "tanh" => MathFunction::Tanh,
                                    "acos" => MathFunction::Acos,
                                    "asin" => MathFunction::Asin,
                                    "log" => MathFunction::Log,
                                    "log2" => MathFunction::Log2,
                                    "length" => MathFunction::Length,
                                    "determinant" => MathFunction::Determinant,
                                    "bitCount" => MathFunction::CountOneBits,
                                    "bitfieldReverse" => MathFunction::ReverseBits,
                                    _ => unreachable!(),
                                },
                                arg: args[0].0,
                                arg1: None,
                                arg2: None,
                            },
                            body,
                        )))
                    }
                    "atan" => {
                        let expr = match args.len() {
                            1 => Expression::Math {
                                fun: MathFunction::Atan,
                                arg: args[0].0,
                                arg1: None,
                                arg2: None,
                            },
                            2 => Expression::Math {
                                fun: MathFunction::Atan2,
                                arg: args[0].0,
                                arg1: Some(args[1].0),
                                arg2: None,
                            },
                            _ => {
                                return Err(ErrorKind::wrong_function_args(
                                    name,
                                    2,
                                    args.len(),
                                    meta,
                                ))
                            }
                        };
                        Ok(Some(ctx.add_expression(expr, body)))
                    }
                    "mod" => {
                        if args.len() != 2 {
                            return Err(ErrorKind::wrong_function_args(name, 2, args.len(), meta));
                        }

                        let (mut left, left_meta) = args[0];
                        let (mut right, right_meta) = args[1];

                        ctx.binary_implicit_conversion(
                            self, &mut left, left_meta, &mut right, right_meta,
                        )?;

                        Ok(Some(ctx.add_expression(
                            Expression::Binary {
                                op: BinaryOperator::Modulo,
                                left,
                                right,
                            },
                            body,
                        )))
                    }
                    "pow" | "dot" | "max" | "min" | "reflect" | "cross" | "outerProduct"
                    | "distance" | "step" | "modf" | "frexp" | "ldexp" => {
                        if args.len() != 2 {
                            return Err(ErrorKind::wrong_function_args(name, 2, args.len(), meta));
                        }
                        Ok(Some(ctx.add_expression(
                            Expression::Math {
                                fun: match name.as_str() {
                                    "pow" => MathFunction::Pow,
                                    "dot" => MathFunction::Dot,
                                    "max" => MathFunction::Max,
                                    "min" => MathFunction::Min,
                                    "reflect" => MathFunction::Reflect,
                                    "cross" => MathFunction::Cross,
                                    "outerProduct" => MathFunction::Outer,
                                    "distance" => MathFunction::Distance,
                                    "step" => MathFunction::Step,
                                    "modf" => MathFunction::Modf,
                                    "frexp" => MathFunction::Frexp,
                                    "ldexp" => MathFunction::Ldexp,
                                    _ => unreachable!(),
                                },
                                arg: args[0].0,
                                arg1: Some(args[1].0),
                                arg2: None,
                            },
                            body,
                        )))
                    }
                    "mix" => {
                        if args.len() != 3 {
                            return Err(ErrorKind::wrong_function_args(name, 3, args.len(), meta));
                        }
                        Ok(Some(
                            if let Some(ScalarKind::Bool) =
                                self.resolve_type(ctx, args[2].0, args[2].1)?.scalar_kind()
                            {
                                ctx.add_expression(
                                    Expression::Select {
                                        condition: args[2].0,
                                        accept: args[0].0,
                                        reject: args[1].0,
                                    },
                                    body,
                                )
                            } else {
                                ctx.add_expression(
                                    Expression::Math {
                                        fun: MathFunction::Mix,
                                        arg: args[0].0,
                                        arg1: Some(args[1].0),
                                        arg2: Some(args[2].0),
                                    },
                                    body,
                                )
                            },
                        ))
                    }
                    "clamp" | "faceforward" | "refract" | "fma" | "smoothstep" => {
                        if args.len() != 3 {
                            return Err(ErrorKind::wrong_function_args(name, 3, args.len(), meta));
                        }
                        Ok(Some(ctx.add_expression(
                            Expression::Math {
                                fun: match name.as_str() {
                                    "clamp" => MathFunction::Clamp,
                                    "faceforward" => MathFunction::FaceForward,
                                    "refract" => MathFunction::Refract,
                                    "fma" => MathFunction::Fma,
                                    "smoothstep" => MathFunction::SmoothStep,
                                    _ => unreachable!(),
                                },
                                arg: args[0].0,
                                arg1: Some(args[1].0),
                                arg2: Some(args[2].0),
                            },
                            body,
                        )))
                    }
                    "lessThan" | "greaterThan" | "lessThanEqual" | "greaterThanEqual" | "equal"
                    | "notEqual" => {
                        if args.len() != 2 {
                            return Err(ErrorKind::wrong_function_args(name, 2, args.len(), meta));
                        }
                        Ok(Some(ctx.add_expression(
                            Expression::Binary {
                                op: match name.as_str() {
                                    "lessThan" => BinaryOperator::Less,
                                    "greaterThan" => BinaryOperator::Greater,
                                    "lessThanEqual" => BinaryOperator::LessEqual,
                                    "greaterThanEqual" => BinaryOperator::GreaterEqual,
                                    "equal" => BinaryOperator::Equal,
                                    "notEqual" => BinaryOperator::NotEqual,
                                    _ => unreachable!(),
                                },
                                left: args[0].0,
                                right: args[1].0,
                            },
                            body,
                        )))
                    }
                    "isinf" | "isnan" | "all" | "any" => {
                        let fun = match name.as_str() {
                            "isinf" => RelationalFunction::IsInf,
                            "isnan" => RelationalFunction::IsNan,
                            "all" => RelationalFunction::All,
                            "any" => RelationalFunction::Any,
                            _ => unreachable!(),
                        };

                        Ok(Some(
                            self.parse_relational_fun(ctx, body, name, &args, fun, meta)?,
                        ))
                    }
                    _ => {
                        let declarations = self.lookup_function.get(&name).ok_or_else(|| {
                            ErrorKind::SemanticError(
                                meta,
                                format!("Unknown function '{}'", name).into(),
                            )
                        })?;

                        let mut maybe_decl = None;
                        let mut ambiguous = false;

                        'outer: for decl in declarations {
                            if args.len() != decl.parameters.len() {
                                continue;
                            }

                            let mut exact = true;

                            for (decl_arg, call_arg) in decl.parameters.iter().zip(args.iter()) {
                                let decl_inner = &self.module.types[*decl_arg].inner;
                                let call_inner = self.resolve_type(ctx, call_arg.0, call_arg.1)?;

                                if decl_inner != call_inner {
                                    exact = false;

                                    match (
                                        decl_inner.scalar_kind().and_then(type_power),
                                        call_inner.scalar_kind().and_then(type_power),
                                    ) {
                                        (Some(decl_power), Some(call_power)) => {
                                            if decl_power < call_power {
                                                continue 'outer;
                                            }
                                        }
                                        _ => continue 'outer,
                                    }
                                }
                            }

                            if exact {
                                maybe_decl = Some(decl);
                                ambiguous = false;
                                break;
                            } else if maybe_decl.is_some() {
                                ambiguous = true;
                            } else {
                                maybe_decl = Some(decl)
                            }
                        }

                        if ambiguous {
                            return Err(ErrorKind::SemanticError(
                                meta,
                                format!("Ambiguous best function for '{}'", name).into(),
                            ));
                        }

                        let decl = maybe_decl.ok_or_else(|| {
                            ErrorKind::SemanticError(
                                meta,
                                format!("Unknown function '{}'", name).into(),
                            )
                        })?;

                        let qualifiers = decl.qualifiers.clone();
                        let parameters = decl.parameters.clone();
                        let function = decl.handle;
                        let is_void = decl.void;

                        let mut arguments = Vec::with_capacity(args.len());
                        let mut proxy_writes = Vec::new();
                        for (qualifier, (expr, parameter)) in qualifiers
                            .iter()
                            .zip(raw_args.iter().zip(parameters.iter()))
                        {
                            let (mut handle, meta) =
                                ctx.lower_expect(self, *expr, qualifier.is_lhs(), body)?;

                            if let TypeInner::Vector { size, kind, width } =
                                *self.resolve_type(ctx, handle, meta)?
                            {
                                if qualifier.is_lhs()
                                    && matches!(
                                        *ctx.get_expression(handle),
                                        Expression::Swizzle { .. }
                                    )
                                {
                                    let ty = self.module.types.append(Type {
                                        name: None,
                                        inner: TypeInner::Vector { size, kind, width },
                                    });
                                    let temp_var = ctx.locals.append(LocalVariable {
                                        name: None,
                                        ty,
                                        init: None,
                                    });
                                    let temp_expr = ctx
                                        .add_expression(Expression::LocalVariable(temp_var), body);

                                    body.push(Statement::Store {
                                        pointer: temp_expr,
                                        value: handle,
                                    });

                                    arguments.push(temp_expr);
                                    proxy_writes.push((*expr, temp_expr));
                                    continue;
                                }
                            }

                            if let Some(kind) = self.module.types[*parameter].inner.scalar_kind() {
                                ctx.implicit_conversion(self, &mut handle, meta, kind)?;
                            }

                            arguments.push(handle)
                        }

                        ctx.emit_flush(body);

                        let result = if !is_void {
                            Some(ctx.add_expression(Expression::Call(function), body))
                        } else {
                            None
                        };

                        body.push(crate::Statement::Call {
                            function,
                            arguments,
                            result,
                        });

                        ctx.emit_start();
                        for (tgt, pointer) in proxy_writes {
                            let temp_ref = ctx.hir_exprs.append(HirExpr {
                                kind: HirExprKind::Variable(VariableReference {
                                    expr: pointer,
                                    load: true,
                                    mutable: true,
                                    entry_arg: None,
                                }),
                                meta,
                            });
                            let assign = ctx.hir_exprs.append(HirExpr {
                                kind: HirExprKind::Assign {
                                    tgt,
                                    value: temp_ref,
                                },
                                meta,
                            });

                            let _ = ctx.lower_expect(self, assign, false, body)?;
                        }
                        ctx.emit_flush(body);
                        ctx.emit_start();

                        Ok(result)
                    }
                }
            }
        }
    }

    pub fn parse_relational_fun(
        &mut self,
        ctx: &mut Context,
        body: &mut Block,
        name: String,
        args: &[(Handle<Expression>, SourceMetadata)],
        fun: RelationalFunction,
        meta: SourceMetadata,
    ) -> Result<Handle<Expression>, ErrorKind> {
        if args.len() != 1 {
            return Err(ErrorKind::wrong_function_args(name, 1, args.len(), meta));
        }

        Ok(ctx.add_expression(
            Expression::Relational {
                fun,
                argument: args[0].0,
            },
            body,
        ))
    }

    pub fn add_function(
        &mut self,
        mut function: Function,
        name: String,
        // Normalized function parameters, modifiers are not applied
        parameters: Vec<Handle<Type>>,
        qualifiers: Vec<ParameterQualifier>,
        meta: SourceMetadata,
    ) -> Result<Handle<Function>, ErrorKind> {
        ensure_block_returns(&mut function.body);
        let stage = self.entry_points.get(&name);

        Ok(if let Some(&stage) = stage {
            let handle = self.module.functions.append(function);
            self.entries.push((name, stage, handle));
            self.function_arg_use.push(Vec::new());
            handle
        } else {
            let void = function.result.is_none();

            let &mut Program {
                ref mut lookup_function,
                ref mut module,
                ..
            } = self;

            let declarations = lookup_function.entry(name).or_default();

            'outer: for decl in declarations.iter_mut() {
                if parameters.len() != decl.parameters.len() {
                    continue;
                }

                for (new_parameter, old_parameter) in parameters.iter().zip(decl.parameters.iter())
                {
                    let new_inner = &module.types[*new_parameter].inner;
                    let old_inner = &module.types[*old_parameter].inner;

                    if new_inner != old_inner {
                        continue 'outer;
                    }
                }

                if decl.defined {
                    return Err(ErrorKind::SemanticError(
                        meta,
                        "Function already defined".into(),
                    ));
                }

                decl.defined = true;
                decl.qualifiers = qualifiers;
                *self.module.functions.get_mut(decl.handle) = function;
                return Ok(decl.handle);
            }

            self.function_arg_use.push(Vec::new());
            let handle = module.functions.append(function);
            declarations.push(FunctionDeclaration {
                parameters,
                qualifiers,
                handle,
                defined: true,
                void,
            });
            handle
        })
    }

    pub fn add_prototype(
        &mut self,
        function: Function,
        name: String,
        // Normalized function parameters, modifiers are not applied
        parameters: Vec<Handle<Type>>,
        qualifiers: Vec<ParameterQualifier>,
        meta: SourceMetadata,
    ) -> Result<(), ErrorKind> {
        let void = function.result.is_none();

        let &mut Program {
            ref mut lookup_function,
            ref mut module,
            ..
        } = self;

        let declarations = lookup_function.entry(name).or_default();

        'outer: for decl in declarations.iter_mut() {
            if parameters.len() != decl.parameters.len() {
                continue;
            }

            for (new_parameter, old_parameter) in parameters.iter().zip(decl.parameters.iter()) {
                let new_inner = &module.types[*new_parameter].inner;
                let old_inner = &module.types[*old_parameter].inner;

                if new_inner != old_inner {
                    continue 'outer;
                }
            }

            return Err(ErrorKind::SemanticError(
                meta,
                "Prototype already defined".into(),
            ));
        }

        self.function_arg_use.push(Vec::new());
        let handle = module.functions.append(function);
        declarations.push(FunctionDeclaration {
            parameters,
            qualifiers,
            handle,
            defined: false,
            void,
        });

        Ok(())
    }

    fn check_call_global(
        &self,
        caller: Handle<Function>,
        function_arg_use: &mut [Vec<EntryArgUse>],
        stmt: &Statement,
    ) {
        match *stmt {
            Statement::Block(ref block) => {
                for stmt in block {
                    self.check_call_global(caller, function_arg_use, stmt)
                }
            }
            Statement::If {
                ref accept,
                ref reject,
                ..
            } => {
                for stmt in accept.iter().chain(reject.iter()) {
                    self.check_call_global(caller, function_arg_use, stmt)
                }
            }
            Statement::Switch {
                ref cases,
                ref default,
                ..
            } => {
                for stmt in cases
                    .iter()
                    .flat_map(|c| c.body.iter())
                    .chain(default.iter())
                {
                    self.check_call_global(caller, function_arg_use, stmt)
                }
            }
            Statement::Loop {
                ref body,
                ref continuing,
            } => {
                for stmt in body.iter().chain(continuing.iter()) {
                    self.check_call_global(caller, function_arg_use, stmt)
                }
            }
            Statement::Call { function, .. } => {
                let callee_len = function_arg_use[function.index()].len();
                let caller_len = function_arg_use[caller.index()].len();
                function_arg_use[caller.index()].extend(
                    std::iter::repeat(EntryArgUse::empty())
                        .take(callee_len.saturating_sub(caller_len)),
                );

                for i in 0..callee_len.min(caller_len) {
                    let callee_use = function_arg_use[function.index()][i];
                    function_arg_use[caller.index()][i] |= callee_use
                }
            }
            _ => {}
        }
    }

    pub fn add_entry_points(&mut self) {
        let mut function_arg_use = Vec::new();
        std::mem::swap(&mut self.function_arg_use, &mut function_arg_use);

        for (handle, function) in self.module.functions.iter() {
            for stmt in function.body.iter() {
                self.check_call_global(handle, &mut function_arg_use, stmt)
            }
        }

        for (name, stage, function) in self.entries.iter().cloned() {
            let mut arguments = Vec::new();
            let mut expressions = Arena::new();
            let mut body = Vec::new();

            for (i, arg) in self.entry_args.iter().enumerate() {
                if function_arg_use[function.index()]
                    .get(i)
                    .map_or(true, |u| !u.contains(EntryArgUse::READ))
                    || !arg.prologue.contains(stage.into())
                {
                    continue;
                }

                let ty = self.module.global_variables[arg.handle].ty;
                let idx = arguments.len() as u32;

                arguments.push(FunctionArgument {
                    name: arg.name.clone(),
                    ty,
                    binding: Some(arg.binding.clone()),
                });

                let pointer = expressions.append(Expression::GlobalVariable(arg.handle));
                let value = expressions.append(Expression::FunctionArgument(idx));

                body.push(Statement::Store { pointer, value });
            }

            body.push(Statement::Call {
                function,
                arguments: Vec::new(),
                result: None,
            });

            let mut span = 0;
            let mut members = Vec::new();
            let mut components = Vec::new();

            for (i, arg) in self.entry_args.iter().enumerate() {
                if function_arg_use[function.index()]
                    .get(i)
                    .map_or(true, |u| !u.contains(EntryArgUse::WRITE))
                {
                    continue;
                }

                let ty = self.module.global_variables[arg.handle].ty;

                members.push(StructMember {
                    name: arg.name.clone(),
                    ty,
                    binding: Some(arg.binding.clone()),
                    offset: span,
                });

                span += self.module.types[ty].inner.span(&self.module.constants);

                let pointer = expressions.append(Expression::GlobalVariable(arg.handle));
                let len = expressions.len();
                let load = expressions.append(Expression::Load { pointer });
                body.push(Statement::Emit(expressions.range_from(len)));
                components.push(load)
            }

            let (ty, value) = if !components.is_empty() {
                let ty = self.module.types.append(Type {
                    name: None,
                    inner: TypeInner::Struct {
                        top_level: false,
                        members,
                        span,
                    },
                });

                let len = expressions.len();
                let res = expressions.append(Expression::Compose { ty, components });
                body.push(Statement::Emit(expressions.range_from(len)));

                (Some(ty), Some(res))
            } else {
                (None, None)
            };

            body.push(Statement::Return { value });

            self.module.entry_points.push(EntryPoint {
                name,
                stage,
                early_depth_test: Some(crate::EarlyDepthTest { conservative: None })
                    .filter(|_| self.early_fragment_tests && stage == crate::ShaderStage::Fragment),
                workgroup_size: if let crate::ShaderStage::Compute = stage {
                    self.workgroup_size
                } else {
                    [0; 3]
                },
                function: Function {
                    arguments,
                    expressions,
                    body,
                    result: ty.map(|ty| FunctionResult { ty, binding: None }),
                    ..Default::default()
                },
            });
        }
    }
}
