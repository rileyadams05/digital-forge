// Opt-in integration test: invokes the real production publication method.
// Arguments: GP4, reference PKG, publisher executable, new diagnostic directory.
// Uses user-supplied legitimate fixtures; never pretends a reduced fixture is a game.
using OrbisPkgTool;
using OrbisPkgTool.Gp4;
using OrbisPkgTool.Pfs;

try
{
if (args.Length == 2 && args[0] == "--validate-inner-no-temp")
{
    // A file (not a directory) as TEMP makes any attempted scratch-image
    // creation fail. The actual production metadata check must not need it.
    var blockedTemp = Path.GetTempFileName();
    var previousTemp = Environment.GetEnvironmentVariable("TEMP");
    var previousTmp = Environment.GetEnvironmentVariable("TMP");
    try
    {
        Environment.SetEnvironmentVariable("TEMP", blockedTemp);
        Environment.SetEnvironmentVariable("TMP", blockedTemp);
        using var package = new PkgReader(Path.GetFullPath(args[1]));
        OrbisPkgTool.Pkg.PkgValidator.ValidateInnerPfs(Path.GetFullPath(args[1]), package);
        Console.WriteLine("PASS: production inner-filesystem checks succeed without writable TEMP or a duplicate payload");
    }
    finally
    {
        Environment.SetEnvironmentVariable("TEMP", previousTemp);
        Environment.SetEnvironmentVariable("TMP", previousTmp);
        File.Delete(blockedTemp);
    }
    return;
}
if (args.Length is not (4 or 5)) throw new ArgumentException("GP4 reference.pkg publisher.exe diagnostic-directory [--regenerate]");
var work = Path.GetFullPath(args[3]);
Directory.CreateDirectory(work);
var output = Path.Combine(work, "publisher-regression.pkg");
if (File.Exists(output)) throw new IOException("Use a new diagnostic directory; output already exists.");
using var source = new PkgReader(Path.GetFullPath(args[1]));
var expected = source.GetInfo();
var projectPath = Path.GetFullPath(args[0]);
if (args.Length == 5)
{
    if (args[4] != "--regenerate") throw new ArgumentException("Unknown option");
    // Reuse a failed job's already-extracted payload without another full copy.
    // Preserve its generated GP4 outside the payload so it cannot be packaged.
    var previous = Gp4Project.Parse(File.ReadAllText(projectPath));
    var policy = previous.Files.ToDictionary(f => f.TargPath,
        f => f.PfsCompression == "disable" ? PfscPolicy.Disable : PfscPolicy.Enable,
        StringComparer.OrdinalIgnoreCase);
    File.Move(projectPath, Path.Combine(work, "before.gp4"));
    var regenerated = Gp4Project.FromFolder(Path.GetDirectoryName(projectPath)!, false,
        expected.Title, expected.TitleId, expected.ContentId, PkgReader.DefaultPasscode,
        policy, strictGengp4: true);
    File.WriteAllText(projectPath, regenerated.Serialize());
    Console.WriteLine($"Production generator: {previous.Files.Count} -> {regenerated.Files.Count} project inputs");
}
var request = new PkgMergeRequest {
    BasePkgPath = args[1], UpdatePkgPath = args[1],
    PublisherToolPath = Path.GetFullPath(args[2]), OutputPkgPath = output
};
PkgMergeService.BuildWithOrbisPublisher(request, projectPath,
    work, output, expected, CancellationToken.None);
using var result = new PkgReader(output);
var actual = result.GetInfo();
if (actual.ContentId != expected.ContentId || actual.TitleId != expected.TitleId ||
    actual.Type != PkgType.Game || actual.Category != "gd")
    throw new Exception("Published package identity mismatch");
foreach (var file in new[] { "publisher-command.json", "publisher-stdout.log", "publisher-stderr.log", "publisher-exit-code.txt" })
    if (!File.Exists(Path.Combine(work, file))) throw new Exception("Missing diagnostic: " + file);
if (File.ReadAllText(Path.Combine(work, "publisher-exit-code.txt")) != "0") throw new Exception("Publisher failed");
Console.WriteLine("PASS: real publisher completed; versioned output discovered and identity verified; diagnostics captured.");
Console.WriteLine(output);
}
catch (Exception error)
{
    Console.Error.WriteLine(error);
    Environment.ExitCode = 1;
}
