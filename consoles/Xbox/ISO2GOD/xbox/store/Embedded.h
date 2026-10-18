#pragma once
#include <xtl.h>
#include <string>
#include <vector>
#include <stdio.h>

static void ResourceStatus(const char* phase,DWORD result) {
    FILE* file=NULL;
    if(fopen_s(&file,"game:\\startup-status.txt","ab")==0 && file) {
        fprintf(file,"%s: 0x%08lX\n",phase,result); fflush(file); fclose(file);
    }
}

static bool Section(const char* name,void** bytes,ULONG* size) {
    return XGetModuleSection(GetModuleHandle(NULL),name,bytes,size)!=FALSE;
}

static bool WriteSection(const char* name,const std::string& path) {
    void* bytes=NULL; ULONG size=0;
    if(!Section(name,&bytes,&size)) return false;
    HANDLE file=CreateFile(path.c_str(),GENERIC_WRITE,0,NULL,CREATE_ALWAYS,FILE_ATTRIBUTE_NORMAL,NULL);
    if(file==INVALID_HANDLE_VALUE) return false;
    DWORD written=0; bool ok=WriteFile(file,bytes,size,&written,NULL)!=FALSE && written==size;
    FlushFileBuffers(file); CloseHandle(file); return ok;
}

static bool Exists(const std::string& path) { return GetFileAttributes(path.c_str())!=0xffffffff; }

// Mount in the application's namespace: System?? aliases belong to system
// processes and were not visible to the Store's Win32 file calls.
static void MountArtworkDrives(std::vector<std::string>& drives) {
    struct AnsiString { USHORT length,maximum; char* buffer; };
    typedef DWORD (*Link)(AnsiString*,AnsiString*);
    Link link=(Link)GetProcAddress(GetModuleHandle("xboxkrnl.exe"),(LPCSTR)259);
    if(!link) { ResourceStatus("artwork mount export unavailable",GetLastError()); return; }
    const char* devices[]={"\\Device\\Harddisk0\\Partition1","\\Device\\Mass0","\\Device\\Mass1","\\Device\\Mass2"};
    for(int i=0;i<4;++i) {
        char alias[64]; sprintf_s(alias,"\\??\\I2GDrive%d:",i);
        AnsiString name={(USHORT)strlen(alias),(USHORT)(strlen(alias)+1),alias};
        AnsiString device={(USHORT)strlen(devices[i]),(USHORT)(strlen(devices[i])+1),(char*)devices[i]};
        DWORD status=link(&name,&device);
        ResourceStatus("artwork mount status",status);
        char root[32]; sprintf_s(root,"I2GDrive%d:\\",i);
        if(Exists(root)) drives.push_back(root);
    }
    ResourceStatus("artwork mounted drive count",(DWORD)drives.size());
}

static void FindAurora(const std::string& root,int depth,unsigned& budget,std::vector<std::string>& found) {
    if(Exists(root+"Aurora.xex")) { found.push_back(root); return; }
    if(depth==0 || budget==0) return;
    WIN32_FIND_DATA entry;
    HANDLE search=FindFirstFile((root+"*").c_str(),&entry);
    if(search==INVALID_HANDLE_VALUE) return;
    do {
        if(!budget) break;
        --budget;
        if(!(entry.dwFileAttributes&FILE_ATTRIBUTE_DIRECTORY) || entry.cFileName[0]=='.') continue;
        // Game libraries cannot contain the dashboard installation.
        if(!_stricmp(entry.cFileName,"Content") || !_stricmp(entry.cFileName,"Games") ||
           !_stricmp(entry.cFileName,"Data") || !_stricmp(entry.cFileName,"Cache")) continue;
        FindAurora(root+entry.cFileName+"\\",depth-1,budget,found);
    } while(FindNextFile(search,&entry));
    FindClose(search);
}

static void InstallEmbeddedArtwork() {
    std::vector<std::string> drives, dashboards;
    MountArtworkDrives(drives);
    for(size_t i=0;i<drives.size();++i) {
        // First follow the actual DashLaunch path, including deeply nested installs.
        FILE* ini=NULL;
        if(fopen_s(&ini,(drives[i]+"launch.ini").c_str(),"r")==0 && ini) {
            char line[1024];
            while(fgets(line,sizeof(line),ini)) {
                std::string value=line;
                size_t end=value.find_last_not_of(" \t\r\n");
                if(end==std::string::npos) continue;
                value.erase(end+1);
                size_t colon=value.find(':');
                if(colon==std::string::npos || value.size()<10 || _stricmp(value.c_str()+value.size()-10,"Aurora.xex")) continue;
                std::string relative=value.substr(colon+1);
                while(!relative.empty() && (relative[0]=='\\' || relative[0]=='/')) relative.erase(0,1);
                for(size_t d=0;d<drives.size();++d) {
                    std::string path=drives[d]+relative;
                    if(Exists(path)) dashboards.push_back(path.substr(0,path.size()-10));
                }
            }
            fclose(ini);
        }
        unsigned budget=1024;
        FindAurora(drives[i],3,budget,dashboards);
    }
    for(size_t i=0;i<dashboards.size();++i) {
        std::string directory=dashboards[i]+"User";
        CreateDirectory(directory.c_str(),NULL); directory+="\\Import";
        CreateDirectory(directory.c_str(),NULL); directory+="\\D3600001";
        CreateDirectory(directory.c_str(),NULL);
        // Aurora consumes its import cache itself when the dashboard is opened.
        ResourceStatus("artwork cover staged",WriteSection("COVER",directory+"\\cover.png"));
        ResourceStatus("artwork icon staged",WriteSection("ICON",directory+"\\icon.png"));
        // PNG import is not automatic in all Aurora versions. Install the actual
        // .asset cache for this title only, using Aurora's TitleId_ContentId layout.
        std::string cache=dashboards[i]+"Data\\GameData\\";
        WIN32_FIND_DATA entry;
        HANDLE search=FindFirstFile((cache+"D3600001_*").c_str(),&entry);
        if(search!=INVALID_HANDLE_VALUE) {
            do {
                if(!(entry.dwFileAttributes&FILE_ATTRIBUTE_DIRECTORY)) continue;
                std::string title=cache+entry.cFileName+"\\";
                ResourceStatus("dashboard cover installed",WriteSection("BOXART",title+"GCD3600001.asset"));
                ResourceStatus("dashboard icon installed",WriteSection("TILEICON",title+"GLD3600001.asset"));
            } while(FindNextFile(search,&entry));
            FindClose(search);
        }
    }
    ResourceStatus("artwork dashboard count",(DWORD)dashboards.size());
}
