"""Bazel tests for the CI's JavaScript actions (ADR-022).

`node_test` runs `node --test` over JavaScript test files on the Node
toolchain MODULE.bazel pins, as an ordinary Bazel test on Linux, macOS and
Windows. `node_syntax_test` runs `node --check` over every JavaScript file
in the package.

The launcher is the only platform-specific part: a two-line `sh` script on
Linux and macOS, a batch file on Windows, where a test cannot assume a
shell. Both start Node from the test's working directory, which Bazel sets
to the main repository's directory in the runfiles tree, so every path in
them is a runfiles-relative `short_path` (an external file's starts with
`../<repo>/`). Windows gets that tree from `build:windows --enable_runfiles`
in .bazelrc.
"""

_NODE_TOOLCHAIN = "@rules_nodejs//nodejs:runtime_toolchain_type"

# `node --test` reads each path as a glob pattern, so a test file whose name
# holds a pattern character would run other files, or none. cmd.exe expands
# `%` and ends a quoted argument at `"`; the `sh` launcher quotes in `'`.
_UNSAFE = "*?[]{}!%\"'"

_NODE_FLAGS = [
    "--test",
    "--test-reporter=spec",
    # Resolve imports from where a file sits in the runfiles tree rather than
    # from the source file its symlink points at, so a test that imports a
    # file missing from `data` fails here as it would on Windows, where the
    # runfiles are copies.
    "--preserve-symlinks",
    "--preserve-symlinks-main",
]

def _node_test_impl(ctx):
    node = ctx.toolchains[_NODE_TOOLCHAIN].nodeinfo.node
    tests = [f.short_path for f in ctx.files.srcs]
    for path in tests:
        for char in _UNSAFE.elems():
            if char in path:
                fail("%s: test file %s holds %r, which node --test or cmd.exe would misread" % (ctx.label, path, char))
    windows = ctx.target_platform_has_constraint(ctx.attr._windows[platform_common.ConstraintValueInfo])
    if windows:
        launcher = ctx.actions.declare_file(ctx.label.name + ".bat")
        command = " ".join(['"%s"' % node.short_path.replace("/", "\\")] + _NODE_FLAGS + ['"%s"' % t for t in tests])
        content = "@echo off\r\n%s\r\nexit /b %%ERRORLEVEL%%\r\n" % command
    else:
        launcher = ctx.actions.declare_file(ctx.label.name + ".sh")
        command = " ".join(["'%s'" % node.short_path] + _NODE_FLAGS + ["'%s'" % t for t in tests])
        content = "#!/bin/sh\nexec %s\n" % command
    ctx.actions.write(launcher, content, is_executable = True)
    runfiles = ctx.runfiles(files = [node] + ctx.files.srcs + ctx.files.data)
    return [DefaultInfo(executable = launcher, runfiles = runfiles)]

node_test = rule(
    implementation = _node_test_impl,
    doc = "Runs `node --test` over `srcs`; `data` holds what they import or read.",
    attrs = {
        "data": attr.label_list(allow_files = True),
        "srcs": attr.label_list(allow_files = [".mjs", ".js", ".cjs"], mandatory = True),
        "_windows": attr.label(default = "@platforms//os:windows"),
    },
    test = True,
    toolchains = [_NODE_TOOLCHAIN],
)

def node_syntax_test(name, src, **kwargs):
    """`node --check` over every JavaScript file in this package.

    Tests only load the files they import, and an action's entry file runs on
    load, so no test imports it. Without this check, a syntax error there
    would pass every PR check and first fail where the action runs.

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
        fail("%s has BUILD files below it (%s): declare their tests in %s's BUILD.bazel instead, so node_syntax_test sees every file" % (native.package_name(), ", ".join(nested), native.package_name()))
    node_test(
        name = name,
        srcs = [src],
        # Each extension may match nothing; the test's own floor catches a
        # glob that matches too little.
        data = native.glob(["**/*.js", "**/*.mjs", "**/*.cjs"], exclude = [src], allow_empty = True),
        **kwargs
    )
