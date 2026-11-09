@{
    # Charter: install.ps1 is an interactive console script. Write-Host is the
    # correct cmdlet there - the result a user reads - while Write-Output would
    # leak objects into the pipeline when the script runs via  iex (irm ... | iex).
    ExcludeRules = @('PSAvoidUsingWriteHost')
}
