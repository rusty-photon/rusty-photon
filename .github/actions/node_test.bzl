"""Bazel tests for the CI's JavaScript actions (ADR-022).

`node_test` runs `node --test` over JavaScript test files on the Node
toolchain MODULE.bazel pins, as an ordinary Bazel test on Linux, macOS and
Windows. `node_link_test` checks that every JavaScript file in the package
parses and that its imports resolve, without running any of them.

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
# holds a pattern character, extglob's `(`, `)` and `|` included, would run
# other files, or none. cmd.exe expands `%` and ends a quoted argument at
# `"`; the `sh` launcher quotes in `'`.
_UNSAFE = "*?[]{}()|!%\"'"

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

def _check(label, kind, values):
    for value in values:
        for char in _UNSAFE.elems():
            if char in value:
                fail("%s: %s %s holds %r, which node --test or the launcher would misread" % (label, kind, value, char))

def _node_test_impl(ctx):
    node = ctx.toolchains[_NODE_TOOLCHAIN].nodeinfo.node
    tests = [f.short_path for f in ctx.files.srcs]
    _check(ctx.label, "test file", tests)
    _check(ctx.label, "node flag", ctx.attr.node_flags)
    args = _NODE_FLAGS + ctx.attr.node_flags
    windows = ctx.target_platform_has_constraint(ctx.attr._windows[platform_common.ConstraintValueInfo])
    if windows:
        launcher = ctx.actions.declare_file(ctx.label.name + ".bat")
        command = " ".join(['"%s"' % a for a in [node.short_path.replace("/", "\\")] + args + tests])
        content = "@echo off\r\n%s\r\nexit /b %%ERRORLEVEL%%\r\n" % command
    else:
        launcher = ctx.actions.declare_file(ctx.label.name + ".sh")
        command = " ".join(["'%s'" % a for a in [node.short_path] + args + tests])
        content = "#!/bin/sh\nexec %s\n" % command
    ctx.actions.write(launcher, content, is_executable = True)

    # A `data` target's own runfiles come too, so a tool listed there (a
    # binary with files of its own) can run inside the test.
    runfiles = ctx.runfiles(files = [node] + ctx.files.srcs + ctx.files.data)
    runfiles = runfiles.merge_all([d[DefaultInfo].default_runfiles for d in ctx.attr.data])
    return [DefaultInfo(executable = launcher, runfiles = runfiles)]

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
        # glob that matches too little. The .js and .cjs files are there to
        # be refused: actions are .mjs.
        data = native.glob(["**/*.js", "**/*.mjs", "**/*.cjs"], exclude = [src], allow_empty = True),
        # vm.SourceTextModule, which links a module without running it.
        node_flags = [
            "--experimental-vm-modules",
            "--disable-warning=ExperimentalWarning",
        ],
        **kwargs
    )
