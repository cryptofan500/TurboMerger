Set WshShell = CreateObject("WScript.Shell")
Set FSO = CreateObject("Scripting.FileSystemObject")
ScriptDir = FSO.GetParentFolderName(WScript.ScriptFullName)
VenvPythonW = ScriptDir & "\.venv\Scripts\pythonw.exe"
If FSO.FileExists(VenvPythonW) Then
    WshShell.CurrentDirectory = ScriptDir
    WshShell.Run """" & VenvPythonW & """ -m turbomerger", 0, False
Else
    WshShell.CurrentDirectory = ScriptDir
    WshShell.Run "pythonw -m turbomerger", 0, False
End If
