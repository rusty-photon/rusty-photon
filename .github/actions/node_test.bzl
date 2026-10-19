"""Bazel rules for the CI's JavaScript actions (ADR-022).

`node_test` runs `node --test` over JavaScript test files on the Node
toolchain MODULE.bazel pins, as an ordinary Bazel test on Linux, macOS and
Windows. `node_link_test` checks that every JavaScript file in the package
parses and that its imports resolve, without running any of them.
`node_binary` runs one JavaScript file on that Node with the arguments
`bazel run` passes it: a developer's way into an action's module.

The launcher is the only platform-specific part: a two-line `sh` script on
Linux and macOS, a batch file on Windows, where a test cannot assume a
shell. Both start Node from the working directory Bazel gives a test or a
`bazel run`, the main repository's directory in the runfiles tree, so every
path in them is a runfiles-relative `short_path` (an external file's starts
with `../<repo>/`). Windows gets that tree from `build:windows
--enable_runfiles` in .bazelrc.
"""

_NODE_TOOLCHAIN = "@rules_nodejs//nodejs:runtime_toolchain_type"

# `node --test` reads each path as a glob pattern, so a test file whose name
# holds a pattern character, extglob's `(`, `)` and `|` included, would run
# other files, or none. cmd.exe expands `%` and ends a quoted argument at
# `"`; the `sh` launcher quotes in `'`.
_UNSAFE = "*?[]{}()|!%\"'"

_TEST_FLAGS = [
    "--test",
    "--test-reporter=spec",
]

_RESOLVE_FLAGS = [
    # Resolve imports from where a file sits in the runfiles tree rather than
    # from the source file its symlink points at, so a test that imports a
    # file missing from `data` fails here as it would on Windows, where the
    # runfiles are copies.
    "--preserve-symlinks",
    "--preserve-symlinks-main",
]

def _check(label, kind, values):
    for value in values:
        for char in _UNSAFE.elems():
            if char in value:
                fail("%s: %s %s holds %r, which node --test or the launcher would misread" % (label, kind, value, char))

def _launch(ctx, args, srcs, passthrough):
    """The launcher that starts Node with `args`, and its runfiles.

    With `passthrough`, the launcher hands Node the arguments it was given
    after `args`.
    """
    node = ctx.toolchains[_NODE_TOOLCHAIN].nodeinfo.node
    windows = ctx.target_platform_has_constraint(ctx.attr._windows[platform_common.ConstraintValueInfo])
    if windows:
        launcher = ctx.actions.declare_file(ctx.label.name + ".bat")
        command = " ".join(['"%s"' % a for a in [node.short_path.replace("/", "\\")] + args])
        if passthrough:
            command += " %*"
        content = "@echo off\r\n%s\r\nexit /b %%ERRORLEVEL%%\r\n" % command
    else:
        launcher = ctx.actions.declare_file(ctx.label.name + ".sh")
        command = " ".join(["'%s'" % a for a in [node.short_path] + args])
        if passthrough:
            command += ' "$@"'
        content = "#!/bin/sh\nexec %s\n" % command
    ctx.actions.write(launcher, content, is_executable = True)

    # A `data` target's own runfiles come too, so a tool listed there (a
    # binary with files of its own) can run inside a test.
    runfiles = ctx.runfiles(files = [node] + srcs + ctx.files.data)
    runfiles = runfiles.merge_all([d[DefaultInfo].default_runfiles for d in ctx.attr.data])
    return [DefaultInfo(executable = launcher, runfiles = runfiles)]

def _node_test_impl(ctx):
    tests = [f.short_path for f in ctx.files.srcs]
    _check(ctx.label, "test file", tests)
    _check(ctx.label, "node flag", ctx.attr.node_flags)
    return _launch(ctx, _TEST_FLAGS + _RESOLVE_FLAGS + ctx.attr.node_flags + tests, ctx.files.srcs, passthrough = False)

node_test = rule(
    implementation = _node_test_impl,
    doc = "Runs `node --test` over `srcs`; `data` holds what they import or read.",
    attrs = {
        "data": attr.label_list(allow_files = True),
        "node_flags": attr.string_list(doc = "More flags for Node, after the launcher's own."),
        # .mjs only: link_test refuses any other JavaScript file.
        "srcs": attr.label_list(allow_files = [".mjs"], mandatory = True),
        "_windows": attr.label(default = "@platforms//os:windows"),
    },
    test = True,
    toolchains = [_NODE_TOOLCHAIN],
)

def _node_binary_impl(ctx):
    entry = ctx.file.src.short_path
    _check(ctx.label, "entry file", [entry])
    return _launch(ctx, _RESOLVE_FLAGS + [entry], [ctx.file.src], passthrough = True)

node_binary = rule(
    implementation = _node_binary_impl,
    doc = "Runs `src` on Node with the arguments `bazel run` passes after `--`; `data` holds what it imports or reads.",
    attrs = {
        "data": attr.label_list(allow_files = True),
        "src": attr.label(allow_single_file = [".mjs"], mandatory = True),
        "_windows": attr.label(default = "@platforms//os:windows"),
    },
    executable = True,
    toolchains = [_NODE_TOOLCHAIN],
)

def node_link_test(name, src, **kwargs):
    """Checks that every JavaScript file in this package parses and links.

    Tests only load the files they import, and an action's entry file runs on
    load, so no test imports it. Without this check, a syntax error or a
    wrong import there would pass every PR check and first fail where the
    action runs.

    Every file means the whole package, which is why no directory below it
    may have a BUILD file of its own: `glob` stops at a package boundary, so
    the files beyond it would drop out of the check without a word.

    Args:
      name: the test's name.
      src: the test file, which checks every JavaScript file in its runfiles.
      **kwargs: passed to `node_test`.
    """
    nested = native.subpackages(include = ["**"], allow_empty = True)
    if nested:
        fail("%s has BUILD files below it (%s): declare their tests in %s's BUILD.bazel instead, so node_link_test sees every file" % (native.package_name(), ", ".join(nested), native.package_name()))
    node_test(
        name = name,
        srcs = [src],
        # Each extension may match nothing; the test's own floor catches a
        # glob that matches too little. Every file but .mjs is there to be
        # refused: actions are .mjs, and Node 24 would also run TypeScript,
        # which ADR-022 rules out.
        data = native.glob(
            ["**/*.js", "**/*.mjs", "**/*.cjs", "**/*.ts", "**/*.mts", "**/*.cts"],
            exclude = [src],
            allow_empty = True,
        ),
        # vm.SourceTextModule, which links a module without running it.
        node_flags = [
            "--experimental-vm-modules",
            "--disable-warning=ExperimentalWarning",
        ],
        **kwargs
    )
