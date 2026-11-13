namespace Azemu.DotnetTests;

public static class Wait
{
    /// <summary>Polls until the condition is true or the time is up; returns the last value of the condition.</summary>
    public static async Task<bool> Until(Func<bool> cond, int seconds = 20)
    {
        var end = DateTime.UtcNow.AddSeconds(seconds);
        while (DateTime.UtcNow < end)
        {
            if (cond()) return true;
            await Task.Delay(100);
        }
        return cond();
    }

    public static async Task<bool> UntilAsync(Func<Task<bool>> cond, int seconds = 20)
    {
        var end = DateTime.UtcNow.AddSeconds(seconds);
        while (DateTime.UtcNow < end)
        {
            if (await cond()) return true;
            await Task.Delay(200);
        }
        return await cond();
    }
}
