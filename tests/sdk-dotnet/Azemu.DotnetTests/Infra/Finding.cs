namespace Azemu.DotnetTests;

/// <summary>
/// A test that shows a difference between azemu and Azure. It is skipped (reason starts with "azemu:")
/// so the suite stays green; AZEMU_FINDINGS=1 (run.sh --findings) runs it, and it fails while the finding stands.
/// </summary>
[AttributeUsage(AttributeTargets.Method)]
public sealed class FindingFactAttribute : FactAttribute
{
    public FindingFactAttribute(string id, string reason)
    {
        if (Environment.GetEnvironmentVariable("AZEMU_FINDINGS") != "1")
            Skip = $"azemu: {id}: {reason}";
    }
}

[AttributeUsage(AttributeTargets.Method)]
public sealed class FindingTheoryAttribute : TheoryAttribute
{
    public FindingTheoryAttribute(string id, string reason)
    {
        if (Environment.GetEnvironmentVariable("AZEMU_FINDINGS") != "1")
            Skip = $"azemu: {id}: {reason}";
    }
}
